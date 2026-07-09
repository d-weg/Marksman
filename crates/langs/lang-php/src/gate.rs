//! The PHP gate: `PhpEngine` = the PHPStan CLI for the VERDICT plus phpactor (started lazily)
//! for rename/willRename. The two never trade jobs: phpactor serves the cross-file rewrites
//! (its `willRenameFiles` is real LSP fileOperations), while PHPStan — a batch analyser, not a
//! server — answers the type-check verdict from a materialized overlay.
use ci_core::{Diag, Error, Result, Sandbox};
use ci_edit::GateEngine;
use ci_lsp::LspClient;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::phpactor;

/// The PHPStan analysis level (`--level`). 5 is a common middle ground: type-aware without
/// the strictest generics/mixed rules that would reject a lot of pre-existing code as
/// "breakage" (the baseline diff excuses pre-existing findings, but a lower level keeps the
/// gate focused on the introduced break). Overridable via `$CI_PHPSTAN_LEVEL`.
fn phpstan_level() -> String {
    std::env::var("CI_PHPSTAN_LEVEL").unwrap_or_else(|_| "5".to_string())
}

/// The PHPStan binary: `$CI_PHPSTAN`, else `phpstan`/`vendor/bin/phpstan` on/near the repo,
/// else a PATH `phpstan`. `None` = no gate available.
pub(crate) fn phpstan_binary(root: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CI_PHPSTAN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let vendored = root.join("vendor/bin/phpstan");
    if vendored.is_file() {
        return Some(vendored);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("phpstan");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Materialize `rel` with `content` under `base`, creating parent dirs. Returns the absolute path.
fn write_overlay(base: &Path, rel: &str, content: &str) -> Result<PathBuf> {
    let abs = base.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Driver(format!("phpstan overlay write: {e}")))?;
    }
    std::fs::write(&abs, content).map_err(|e| Error::Driver(format!("phpstan overlay write: {e}")))?;
    Ok(abs)
}

/// Run PHPStan over a materialized mirror of the project — the OVERLAY `files` shadowing the rest
/// of the repo's `.php` on disk — and map the per-file JSON to ERROR [`Diag`]s. Only the overlay
/// files are ANALYSED (the radius the spine handed us); every other project file is mirrored too
/// and fed to PHPStan via `scanDirectories` purely for SYMBOL RESOLUTION. This is the PHP analog
/// of lang-java's `-sourcepath`: PHPStan has no in-memory buffer API, so a coherent project only
/// exists as a temp tree, and a sibling class an analysed file references (`new DocEntry(...)`,
/// `$doc->field`) must be discoverable or PHPStan reports it as an "unknown class" — a FALSE
/// reject on a perfectly good edit (the schema-field bench blew a rust arm to 1M tokens because
/// the touched file was analysed alone and every reference to an unmaterialized sibling read as a
/// new error). Reported paths are relativized back to the repo-relative keys the spine speaks.
fn phpstan_diagnostics(bin: &Path, root: &Path, sandbox: &dyn Sandbox, files: &[(String, String)]) -> Result<Vec<Diag>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let dir = tempfile::tempdir().map_err(|e| Error::Driver(format!("phpstan overlay dir: {e}")))?;
    // The overlay shadows disk: an overlay rel is always taken from `files` (its edited content,
    // or ABSENT when the batch deletes it — empty buffer, the spine's deletion convention), never
    // from the stale disk copy underneath.
    let overlay: std::collections::HashSet<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
    // 1. Mirror every OTHER `.php` in the project (disk content) so sibling symbols resolve.
    //    `ignore::WalkBuilder` honours .gitignore, so vendor/build output stays out of the tree.
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("php") {
            continue;
        }
        let Ok(rel) = p.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || overlay.contains(rel.as_str()) {
            continue; // overlay files are written (or, if deleted, withheld) below
        }
        // A non-gitignored `vendor/` would balloon the mirror with thousands of dependency
        // files — belt-and-suspenders past the .gitignore WalkBuilder already honours (composer
        // projects ignore vendor, but not all do). Skipping them costs no resolution: this
        // configless gate has no autoloader, so vendor symbols were never resolvable anyway —
        // they stay unknown in both baseline and after passes and the diff excuses them.
        if rel.starts_with("vendor/") || rel.contains("/vendor/") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(p) {
            write_overlay(dir.path(), &rel, &content)?;
        }
    }
    // 2. Write the overlay files and collect them as the ANALYSE targets. Deletions (empty
    //    buffers) are NOT materialized — a surviving consumer's reference to the deleted class is
    //    then the unknown symbol PHPStan reports (the introduced break we WANT to catch).
    let mut targets: Vec<PathBuf> = Vec::new();
    for (rel, content) in files.iter().filter(|(_, c)| !c.is_empty()) {
        targets.push(write_overlay(dir.path(), rel, content)?);
    }
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    // 3. A temp config points `scanDirectories` at the whole mirror so PHPStan discovers the
    //    non-analysed siblings without composer/vendor (autoload-less resolution). Neon strings
    //    are single-quoted; a tempdir path never contains a quote.
    let scan_root = dir.path().to_string_lossy().replace('\\', "/");
    let neon = format!(
        "parameters:\n    scanDirectories:\n        - '{scan_root}'\n    tmpDir: '{scan_root}/.phpstan-cache'\n"
    );
    let neon_path = dir.path().join("phpstan-gate.neon");
    std::fs::write(&neon_path, neon).map_err(|e| Error::Driver(format!("phpstan overlay write: {e}")))?;
    let mut cmd = Command::new(bin);
    cmd.args(["analyse", "--error-format=json", "--no-progress", "--no-ansi"])
        .arg("-c")
        .arg(&neon_path)
        .arg(format!("--level={}", phpstan_level()))
        .args(&targets)
        .current_dir(dir.path());
    // Capped + time-bounded like the swift gate: a chatty analyser can't OOM us, and a wedged one
    // can't hang the edit forever (generous timeout — a legit slow analysis is never killed).
    let out = sandbox
        .run_capped(&mut cmd, ci_core::gate_timeout(), 32 * 1024 * 1024)
        .map_err(|e| Error::Driver(format!("phpstan spawn: {e}")))?;
    if out.timed_out {
        return Err(Error::Driver(format!(
            "phpstan exceeded the gate timeout ({}s) — set CI_GATE_TIMEOUT_SECS higher if this project legitimately analyses slower",
            ci_core::gate_timeout().as_secs()
        )));
    }
    // PHPStan exits non-zero WHEN it finds errors — that is the normal reporting path, not a
    // tool failure. Parse stdout regardless.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let diags = parse_phpstan_json(&stdout, dir.path())?;
    // Reject-on-failed-tool (the invariant lang-rust's gate encodes): if PHPStan exits non-zero
    // but we parsed NO diagnostics, it crashed before emitting JSON (segfault, OOM-kill, a fatal
    // in a rule/extension, a bad --level, no PHP runtime) — the message is on stderr and stdout is
    // empty. The spine reads an empty diagnostic set as clean-commit, so surface the failure as a
    // reject rather than let a broken analyser pass silently.
    if !out.status.is_some_and(|s| s.success()) && diags.is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("phpstan failed with no diagnostic output");
        return Ok(vec![Diag { file: "phpstan".into(), code: 0, message: first.to_string(), line: 0 }]);
    }
    Ok(diags)
}

/// Parse PHPStan's `--error-format=json` output into repo-relative [`Diag`]s. The schema is not
/// documented in writing (spec V2), so this is DEFENSIVE: every field is optional-with-fallback,
/// and a shape we don't recognize yields no diagnostics rather than a panic. Observed shape:
/// `{"files": {"<abs path>": {"messages": [{"message": "...", "line": N}]}}, ...}`.
fn parse_phpstan_json(stdout: &str, overlay_root: &Path) -> Result<Vec<Diag>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let v: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        // A non-JSON stdout (a PHP fatal, a config error printed as text) is not something the
        // gate can turn into anchored diagnostics — surface it so the edit isn't silently gated
        // clean on a broken analyser.
        Err(e) => return Err(Error::Driver(format!("phpstan output unparsable as json: {e}"))),
    };
    let prefix = format!("{}/", overlay_root.to_string_lossy().trim_end_matches('/'));
    let mut out = Vec::new();
    if let Some(files) = v.get("files").and_then(|f| f.as_object()) {
        for (path, entry) in files {
            let mut file = path.replace('\\', "/");
            if let Some(rel) = file.strip_prefix(&prefix) {
                file = rel.to_string();
            }
            let Some(messages) = entry.get("messages").and_then(|m| m.as_array()) else { continue };
            for m in messages {
                let message = m.get("message").and_then(|s| s.as_str()).unwrap_or("").to_string();
                if message.is_empty() {
                    continue;
                }
                let line = m.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
                out.push(Diag { file: file.clone(), code: 0, message, line });
            }
        }
    }
    // Top-level (non-file-scoped) `errors`: "Internal error", config/ignore mismatch, reflection
    // failures — PHPStan telling us it could NOT analyze. Dropping these was a false-clean: a run
    // whose only output is a populated top-level `errors` array must still REJECT, never pass.
    if let Some(errors) = v.get("errors").and_then(|e| e.as_array()) {
        for e in errors {
            let message = match e {
                Value::String(s) => s.clone(),
                other => other.get("message").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            };
            if !message.is_empty() {
                out.push(Diag { file: "phpstan".into(), code: 0, message, line: 0 });
            }
        }
    }
    Ok(out)
}

/// The PHP write engine behind `Composed`: PHPStan verdicts, phpactor rewrites.
pub(crate) struct PhpEngine {
    pub(crate) root: PathBuf,
    pub(crate) phpstan: PathBuf,
    /// phpactor, started on the FIRST rename/move only — diagnostics never wait on it, and a
    /// missing phpactor costs nothing until an op actually needs cross-file rewrites.
    pub(crate) lsp: Option<LspClient>,
    /// Where the toolchain runs (`ci_core::resolve_sandbox`). `HostSandbox` today; the one seam a
    /// container backend swaps in — see `docs/container-gate-spec.md`.
    pub(crate) sandbox: Arc<dyn Sandbox>,
}

impl PhpEngine {
    fn phpactor(&mut self) -> Result<&mut LspClient> {
        if self.lsp.is_none() {
            self.lsp = Some(phpactor::start(&self.root, &*self.sandbox)?);
        }
        Ok(self.lsp.as_mut().expect("just set"))
    }
}

/// Diagnostics for references to files the CURRENT BATCH deletes (empty-content buffers, the
/// gate's deletion convention): `use A\B\C;` declarations resolving to a deleted path, via the
/// shared §8 engine over the PHP move hooks. PHPStan's own diagnostics DO report the resulting
/// unknown symbol, but the reject-recipe contract (§5) wants the anchored, ready-to-copy site
/// too — the same gap-fill shape lang-rust/lang-java run.
fn deleted_path_references(root: &Path, files: &[(String, String)]) -> Vec<Diag> {
    ci_edit::moves::deleted_reference_diags(&crate::movefix::PhpMoveModel(root), files)
}

impl GateEngine for PhpEngine {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        let mut out = phpstan_diagnostics(&self.phpstan, &self.root, &*self.sandbox, files)?;
        out.extend(deleted_path_references(&self.root, files));
        Ok(out)
    }

    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<Value> {
        GateEngine::rename(self.phpactor()?, file, line, character, new_name)
    }

    fn will_rename(&mut self, from: &str, to: &str) -> Result<Value> {
        // Engine-native FIRST (contract §8): phpactor's `willRenameFiles` rewrites the class's
        // namespace AND every referencing `use`/FQCN for a PHP move — a complete, project-aware
        // rewrite the syntactic model only approximates. This mirrors lang-java's jdtls ordering
        // (engine-native where it exists, the movefix hooks as the runnable fallback), and the
        // PHPStan gate judges whichever rewrite lands.
        if let Ok(lsp) = self.phpactor() {
            if let Ok(we) = GateEngine::will_rename(lsp, from, to) {
                if !ci_edit::workspace_edit_is_empty(&we) {
                    return Ok(we);
                }
            }
        }
        Ok(crate::movefix::move_workspace_edit(&self.root, from, to).unwrap_or_else(|| json!({})))
    }

    fn sync_disk(&mut self) -> Result<()> {
        // PHPStan holds no cross-call buffers (each analyse materializes its own overlay); only
        // a started phpactor has state to resync.
        match self.lsp.as_mut() {
            Some(lsp) => lsp.sync_disk(),
            None => Ok(()),
        }
    }

    fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        match self.lsp.as_mut() {
            Some(lsp) => lsp.fs_events(created, deleted),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The defensive JSON parser: the observed PHPStan shape maps to repo-relative diagnostics,
    // paths are relativized against the overlay root, and a shape we don't recognize (or an
    // empty run) yields no diagnostics rather than a panic.
    #[test]
    fn phpstan_json_parses_defensively() {
        let overlay = Path::new("/tmp/ov");
        let out = r#"{"totals":{"errors":0,"file_errors":1},"files":{"/tmp/ov/src/Svc.php":{"errors":1,"messages":[{"message":"Method Svc::probe() should return int but returns string.","line":7,"ignorable":true}]}},"errors":[]}"#;
        let diags = parse_phpstan_json(out, overlay).unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "src/Svc.php", "path relativized against the overlay root");
        assert_eq!(diags[0].line, 7);
        assert!(diags[0].message.contains("should return int"));

        // Clean run (no files block / empty) => no diagnostics, no panic.
        assert!(parse_phpstan_json(r#"{"totals":{"errors":0},"files":[],"errors":[]}"#, overlay).unwrap().is_empty());
        assert!(parse_phpstan_json("", overlay).unwrap().is_empty());
        // A non-JSON fatal is surfaced as an error, never silently swallowed.
        assert!(parse_phpstan_json("PHP Fatal error: ...", overlay).is_err());

        // A top-level (non-file-scoped) `errors` entry — PHPStan couldn't analyze — must REJECT,
        // not be dropped as clean. Covers both the string and object message shapes.
        let internal = r#"{"totals":{"errors":1,"file_errors":0},"files":[],"errors":["Internal error: something broke"]}"#;
        let diags = parse_phpstan_json(internal, overlay).unwrap();
        assert_eq!(diags.len(), 1, "top-level error surfaced: {diags:?}");
        assert_eq!(diags[0].file, "phpstan");
        assert!(diags[0].message.contains("Internal error"));
    }
}
