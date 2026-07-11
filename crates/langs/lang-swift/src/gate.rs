//! The Swift gate: `SwiftEngine` = `swift build` for the VERDICT plus sourcekit-lsp (started
//! lazily) for rename. The two never trade jobs: sourcekit-lsp serves the cross-file rename (its
//! rename is index-backed and production-quality since Swift 6), while `swift build` — a batch
//! compiler, not a server — answers the type-check verdict.
//!
//! Why `swift build` and not `swiftc -typecheck`: the Swift core team refutes typecheck-only as a
//! gate because it misses SIL-phase diagnostics (definite-initialization, some exhaustiveness) —
//! an UNSOUND gate, the false-clean failure the contract exists to prevent. `swift build`
//! compiles the SwiftPM package; a broken overlay fails the build. Diagnostics are GCC-style
//! text — compiler errors land on STDOUT, SwiftPM manifest/toolchain failures on stderr (no JSON
//! — the JSON-diagnostics issue is still open upstream); both streams are regex-parsed to
//! file:line:col.
use ci_core::{Diag, Error, Result, Sandbox};
use ci_edit::GateEngine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::sourcekit;

/// Run `swift build` over a materialized MIRROR of the package (the current on-disk tree with the
/// overlay buffers written over it) and map the GCC-style stderr to ERROR [`Diag`]s. `swift build`
/// needs the WHOLE package to compile (unlike PHPStan's per-file analyse), so the mirror is a full
/// copy of `root` with `files` overlaid; deleted files (empty-content buffers, the spine's
/// deletion convention) are removed from the mirror so a surviving consumer's reference to the
/// deleted symbol is the introduced break we WANT to catch.
fn swift_build_diagnostics(
    root: &Path,
    sandbox: &dyn Sandbox,
    files: &[(String, String)],
    target_dirs: Option<&[String]>,
) -> Result<Vec<Diag>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mirror = tempfile::tempdir().map_err(|e| Error::Driver(format!("swift build mirror dir: {e}")))?;
    copy_package_tree(root, mirror.path(), target_dirs)?;
    for (rel, content) in files {
        let abs = mirror.path().join(rel);
        if content.is_empty() {
            // Deletion stand-in: drop it from the mirror so the compiler sees it gone.
            let _ = std::fs::remove_file(&abs);
            continue;
        }
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Driver(format!("swift build overlay write: {e}")))?;
        }
        std::fs::write(&abs, content).map_err(|e| Error::Driver(format!("swift build overlay write: {e}")))?;
    }
    let mut cmd = Command::new("swift");
    cmd.arg("build").current_dir(mirror.path());
    // Capped + time-bounded (run_gate_capped): a chatty build can't OOM us, and a hung one can't
    // hang the edit forever; a timeout REFUSES the edit (Error::GateTimeout propagates).
    let out = ci_core::run_gate_capped(sandbox, &mut cmd, "swift build")?;
    // `swift build` exits non-zero WHEN the build fails — the normal reporting path, not a tool
    // failure. The compiler prints its GCC-style diagnostics to STDOUT (the driver's progress log
    // rides there too); stderr carries SwiftPM-level manifest/toolchain failures. Parse both so a
    // reject site is caught wherever it lands.
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut diags = parse_swift_diagnostics(&combined, mirror.path());
    // Reject-on-failed-tool: a nonzero exit with NO parsed diagnostic means the build failed in a
    // way that carries no `PATH:LINE:COL: error:` line — a link error (`ld: symbol(s) not
    // found`), an invalid manifest, a toolchain fault.
    if let Some(d) = ci_core::silent_tool_failure_diag(
        out.status.is_some_and(|s| s.success()),
        &diags,
        "Package.swift",
        || {
            combined
                .lines()
                .map(str::trim)
                .find(|l| l.contains("error:"))
                .unwrap_or("swift build failed with no source-anchored diagnostic")
                .to_string()
        },
    ) {
        diags.push(d);
    }
    Ok(diags)
}

/// Copy the SwiftPM package tree (`Package.swift` + the `.swift` sources the compiler reads, plus
/// `Package.resolved`) from `src` to `dst`. `.build`/`.git`/hidden dirs are skipped — the mirror
/// only needs the compiler's inputs, and copying `.build` would be enormous and stale. When
/// `target_dirs` is known (from `swift package describe`), `.swift` files OUTSIDE every target are
/// skipped too — `swift build` never compiles them, so copying them is pure overhead (B2). The
/// cold full-package BUILD is the intrinsic cost (the soundness reason for `swift build` over
/// `swiftc -typecheck`); bounding the COPY is the only safe lever.
fn copy_package_tree(src: &Path, dst: &Path, target_dirs: Option<&[String]>) -> Result<()> {
    let under_target = |rel: &str| {
        target_dirs.is_none_or(|dirs| {
            dirs.iter().any(|d| d.is_empty() || rel == d || rel.starts_with(&format!("{d}/")))
        })
    };
    for entry in ignore::WalkBuilder::new(src).hidden(false).git_ignore(false).build().flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(src) else { continue };
        let rel_str = rel.to_string_lossy();
        if rel_str.is_empty() {
            continue;
        }
        // Skip build/VCS output — the compiler reconstructs `.build`, and a copied one is stale.
        if rel.components().any(|c| matches!(c.as_os_str().to_str(), Some(".build") | Some(".git"))) {
            continue;
        }
        let target = dst.join(rel);
        if path.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| Error::Driver(format!("mirror mkdir {rel_str}: {e}")))?;
        } else if path.is_file() {
            let is_swift = path.extension().and_then(|e| e.to_str()) == Some("swift");
            let is_manifest =
                matches!(path.file_name().and_then(|n| n.to_str()), Some("Package.swift") | Some("Package.resolved"));
            let keep = is_manifest || (is_swift && under_target(&rel_str.replace('\\', "/")));
            if !keep {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::Driver(format!("mirror mkdir: {e}")))?;
            }
            std::fs::copy(path, &target).map_err(|e| Error::Driver(format!("mirror copy {rel_str}: {e}")))?;
        }
    }
    Ok(())
}

/// Parse `swift build`'s GCC-style output (combined stdout+stderr) into repo-relative [`Diag`]s.
/// The line shape is `/abs/path.swift:LINE:COL: error: message` (warnings/notes filtered out —
/// only `error:` gates). Paths are relativized against the mirror root so the reject sites line up
/// with the repo-relative keys the spine speaks. DEFENSIVE: a line that doesn't match the shape is
/// skipped, never a panic.
fn parse_swift_diagnostics(combined: &str, mirror_root: &Path) -> Vec<Diag> {
    // The mirror is a per-call tempdir; the baseline-diff keys diagnostics by (file, message), so
    // the reported path MUST relativize to a stable repo-relative key — otherwise a pre-existing
    // error re-parsed under a fresh mirror looks INTRODUCED and false-rejects. macOS reports the
    // canonical `/private/var/...` for a `/var/...` tempdir, so canonicalize before matching and
    // try both spellings.
    let raw = mirror_root.to_string_lossy().trim_end_matches('/').to_string();
    let canon = std::fs::canonicalize(mirror_root)
        .ok()
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string());
    let prefixes: Vec<String> = std::iter::once(format!("{raw}/"))
        .chain(canon.into_iter().map(|c| format!("{c}/")))
        .collect();
    // `swift build` prints the SAME diagnostic once per build phase (emit-module + compile), and
    // the mirror is a fresh tempdir each call so phase multiplicity isn't guaranteed identical
    // between the baseline build and the after build. The spine's baseline-diff counts instances
    // per (file, code, message) key, so an undeduped 3×-vs-2× split reads as an introduced error
    // and false-rejects an untouched edit. Dedup by (file, line, message) → one instance each.
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<(String, u32, String)> = std::collections::HashSet::new();
    for line in combined.lines() {
        let Some((file, rest, lineno)) = split_diag_head(line) else { continue };
        // Only `error:` gates; `warning:`/`note:` never reject.
        let Some(msg) = rest.strip_prefix("error:") else { continue };
        let mut file = file.replace('\\', "/");
        for prefix in &prefixes {
            if let Some(r) = file.strip_prefix(prefix) {
                file = r.to_string();
                break;
            }
        }
        // A diagnostic on a file OUTSIDE the mirror (a toolchain header) isn't an edit site — but
        // the spine's baseline-diff will excuse anything not introduced, so keep it repo-relative
        // only when it is under the mirror; otherwise carry the absolute path as-is.
        let message = msg.trim().to_string();
        if seen.insert((file.clone(), lineno, message.clone())) {
            out.push(Diag { file, code: 0, message, line: lineno });
        }
    }
    out
}

/// Split a GCC-style head `PATH:LINE:COL: <rest>` into `(path, rest_after_the_col_space, line)`.
/// `None` when the line isn't in that shape (a build banner, a `note:` continuation, blank). The
/// path may itself contain `:` only on Windows drive letters, which this build never sees — the
/// LAST two numeric colon-fields before the space are LINE and COL.
fn split_diag_head(line: &str) -> Option<(String, &str, u32)> {
    // Scan each `: ` separator and accept the one whose head ends in `:LINE:COL` (two numeric
    // colon-fields). Anchoring on the numeric shape rather than the FIRST `: ` tolerates a path
    // that itself contains `: ` — the first separator would otherwise mis-split and silently drop
    // the diagnostic. Normal `PATH:LINE:COL: severity: msg` lines resolve identically.
    let mut from = 0;
    while let Some(rel) = line[from..].find(": ") {
        let sep = from + rel;
        let head = &line[..sep];
        if let Some(col_at) = head.rfind(':') {
            if let Some(line_at) = head[..col_at].rfind(':') {
                if let (Ok(lineno), Ok(_col)) =
                    (head[line_at + 1..col_at].parse::<u32>(), head[col_at + 1..].parse::<u32>())
                {
                    let path = head[..line_at].to_string();
                    if !path.is_empty() {
                        return Some((path, line[sep + 2..].trim_start(), lineno));
                    }
                }
            }
        }
        from = sep + 2;
    }
    None
}

/// Build the package AT THE REPO ROOT so `.build/*/index/store` (the IndexStoreDB sourcekit-lsp's
/// rename reads from) is populated. Best-effort — a build failure isn't fatal here (the gate has
/// its own verdict path); the rename simply falls back to whatever index freshness sourcekit has.
/// Skipped when there is no `Package.swift` (sourcekit-lsp is SwiftPM-only anyway). Runs through
/// the SANDBOX (time-bounded): under `CI_SANDBOX=oci` this primes the container's index store —
/// the one sourcekit actually reads there — instead of pointlessly invoking host swift.
fn prime_index(root: &Path, sandbox: &dyn Sandbox) {
    if !root.join("Package.swift").is_file() {
        return;
    }
    let Ok(mut cmd) = ci_core::tool_command(sandbox, "swift", || Ok(Command::new("swift"))) else {
        return;
    };
    cmd.arg("build").current_dir(root);
    let _ = sandbox.run_capped(&mut cmd, ci_core::gate_timeout(), 1024 * 1024);
}

/// The Swift write engine behind `Composed`: `swift build` verdicts, sourcekit-lsp rewrites.
pub(crate) struct SwiftEngine {
    root: PathBuf,
    /// sourcekit-lsp, started on the FIRST rename only (`ci_edit::LazyLsp`) — diagnostics never
    /// wait on it, and a missing LSP costs nothing until an op needs cross-file rewrites.
    lsp: ci_edit::LazyLsp,
    /// Cached SwiftPM target source dirs (repo-relative), from `swift package describe`. Outer
    /// `None` = not probed yet; inner `None` = no Package.swift (not a SwiftPM package — the only
    /// case the G4 target check legitimately skips). A describe FAILURE on a real package is an
    /// `Err` (fail closed) and is NOT cached, so a fixed toolchain recovers on the next edit.
    /// Populated lazily on the first `diagnostics` call.
    target_dirs: Option<Option<Vec<String>>>,
    /// Where the toolchain runs (`ci_core::resolve_sandbox`) — the `swift build` verdict, the
    /// describe probe, and index priming all use it; the container backend swaps in here.
    sandbox: Arc<dyn Sandbox>,
}

impl SwiftEngine {
    pub(crate) fn new(root: &Path, sandbox: Arc<dyn Sandbox>) -> Self {
        let lsp = ci_edit::LazyLsp::new({
            let root = root.to_path_buf();
            let sandbox = sandbox.clone();
            move || sourcekit::start(&root, &*sandbox)
        });
        Self { root: root.to_path_buf(), lsp, target_dirs: None, sandbox }
    }

    /// G4 honesty check: `swift build` only type-checks files that belong to a SwiftPM target, so
    /// an edit to a file OUTSIDE every target would build clean and commit under a false
    /// "type-verified" banner. Swift's import graph is empty, so the gate's `files` set is exactly
    /// the edited files — refuse the batch (an `Err`, which propagates before baseline-diff can
    /// cancel it) when an edited `.swift` file is in no target. Fail CLOSED when describe fails
    /// on a real package (the `?` below): a check that can't run must refuse, not skip.
    fn reject_untargeted(&mut self, files: &[(String, String)]) -> Result<()> {
        if self.target_dirs.is_none() {
            self.target_dirs = Some(describe_target_dirs(&self.root, &*self.sandbox)?);
        }
        let Some(Some(dirs)) = self.target_dirs.as_ref() else { return Ok(()) };
        for (rel, content) in files {
            if content.is_empty() || !rel.ends_with(".swift") {
                continue; // deletions (empty buffers) and non-swift files ride along untouched
            }
            let under = dirs
                .iter()
                .any(|d| d.is_empty() || rel == d || rel.starts_with(&format!("{d}/")));
            if !under {
                return Err(Error::Driver(format!(
                    "`{rel}` is not in any SwiftPM target — `swift build` cannot type-check it, so \
                     this edit cannot be gated. Move it under a target's sources (e.g. \
                     `Sources/<target>/`) or add the target to Package.swift."
                )));
            }
        }
        Ok(())
    }
}

/// The repo-relative source directory of each SwiftPM target, via `swift package describe
/// --type json` (authoritative — it evaluates Package.swift). A `.swift` file under one of these
/// dirs is compiled by `swift build`; one outside all of them is not (the G4 check). Runs through
/// the SANDBOX — under `CI_SANDBOX=oci` the container's swift answers, so the check works exactly
/// where host swift is absent. `Ok(None)` ONLY for "no Package.swift" (legitimately not a SwiftPM
/// package — `swift build` refuses such an edit anyway); a describe failure on a REAL package is
/// an `Err` — fail CLOSED. Guessing "no targets" here would let an ungateable edit sail through
/// under a false type-verified banner, the silent gate degrade the house rules forbid.
fn describe_target_dirs(root: &Path, sandbox: &dyn Sandbox) -> Result<Option<Vec<String>>> {
    if !root.join("Package.swift").is_file() {
        return Ok(None);
    }
    let fail = |why: String| {
        Error::Driver(format!(
            "`swift package describe` failed ({why}) — the SwiftPM target check cannot run, so \
             this edit cannot be gated. Fix Package.swift or the Swift toolchain and retry."
        ))
    };
    let mut cmd = ci_core::tool_command(sandbox, "swift", || Ok(Command::new("swift")))?;
    cmd.args(["package", "describe", "--type", "json"]).current_dir(root);
    let out = ci_core::run_gate_capped(sandbox, &mut cmd, "swift package describe")?;
    if !out.status.is_some_and(|s| s.success()) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first =
            stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("nonzero exit").to_string();
        return Err(fail(first));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| fail(format!("unparsable JSON: {e}")))?;
    let targets = v
        .get("targets")
        .and_then(|t| t.as_array())
        .ok_or_else(|| fail("no `targets` array in the describe output".into()))?;
    let mut dirs = Vec::new();
    for t in targets {
        if let Some(p) = t.get("path").and_then(|p| p.as_str()) {
            let p = p.trim_matches('/').replace('\\', "/");
            dirs.push(if p == "." { String::new() } else { p });
        }
    }
    Ok(Some(dirs))
}

/// Diagnostics for references to files the CURRENT BATCH deletes, via the shared §8 engine over
/// the Swift move hooks. Swift's within-target model means files aren't referenced by path (the
/// `ref_occurrences` hook is a no-op), so this yields nothing today — but wiring it keeps the
/// deletion-soundness path identical to Java/PHP for the day cross-target references land.
fn deleted_path_references(root: &Path, files: &[(String, String)]) -> Vec<Diag> {
    let _ = root; // the Swift move model is rootless (within-target hooks consult no disk)
    ci_edit::moves::deleted_reference_diags(&crate::movefix::SwiftMoveModel, files)
}

impl GateEngine for SwiftEngine {
    fn diagnostics(&mut self, files: &[(String, String)]) -> Result<Vec<Diag>> {
        self.reject_untargeted(files)?; // also populates self.target_dirs (cached describe)
        let dirs = self.target_dirs.as_ref().and_then(|o| o.as_deref());
        let mut out = swift_build_diagnostics(&self.root, &*self.sandbox, files, dirs)?;
        out.extend(deleted_path_references(&self.root, files));
        Ok(out)
    }

    fn rename(&mut self, file: &str, line: u32, character: u32, new_name: &str) -> Result<Value> {
        // sourcekit-lsp's rename is served from the IndexStoreDB (`.build/*/index/store`), which is
        // populated by a BUILD. Background indexing warms it eventually, but on a fresh package the
        // index is empty and rename returns NO edits — or only the DEFINITION, missing cross-file
        // references (not an error, so the generic retry path never fires). Two steps make the
        // rename deterministic (rename correctness = index freshness, the staleness class the Rust
        // gate already handles):
        //   1. build at the REPO ROOT to populate its index store (the gate builds a throwaway
        //      mirror, so the root's index would otherwise stay empty), and
        //   2. drain sourcekit's index queue with `workspace/_pollIndex` so the rename sees every
        //      reference, not just the definition.
        prime_index(&self.root, &*self.sandbox);
        let lsp = self.lsp.get()?;
        // `_pollIndex` blocks until the index is up to date; ignore its (empty) result. Best-effort
        // — an older sourcekit without the request just falls through to whatever freshness it has.
        let _ = lsp.request("workspace/_pollIndex", json!({}));
        GateEngine::rename(lsp, file, line, character, new_name)
    }

    fn will_rename(&mut self, from: &str, to: &str) -> Result<Value> {
        // sourcekit-lsp REFUTES `willRenameFiles` (no handler), so — unlike jdtls/phpactor — there
        // is no engine-native move to try first. The §8 hooks are the whole move story: within a
        // target they are near-no-ops (files aren't referenced by path), and a cross-target move
        // rewrites `Package.swift` membership. The `swift build` gate judges the result.
        Ok(crate::movefix::move_workspace_edit(&self.root, from, to).unwrap_or_else(|| json!({})))
    }

    fn sync_disk(&mut self) -> Result<()> {
        // `swift build` holds no cross-call buffers (each build materializes its own mirror); only
        // a started sourcekit-lsp has state to resync.
        self.lsp.sync_disk()
    }

    fn fs_events(&mut self, created: &[String], deleted: &[String]) -> Result<()> {
        self.lsp.fs_events(created, deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The GCC-style parser: an `error:` line maps to a repo-relative diagnostic (path relativized
    // against the mirror root), and non-error / non-matching lines (warnings, notes, banners)
    // yield nothing — never a panic.
    #[test]
    fn swift_diagnostics_parse_defensively() {
        let mirror = Path::new("/tmp/mir");
        let stderr = "\
Compiling App main.swift
/tmp/mir/Sources/App/main.swift:2:12: error: cannot convert return expression of type 'String' to return type 'Int'
/tmp/mir/Sources/App/main.swift:5:3: warning: variable 'x' was never used
/tmp/mir/Sources/App/main.swift:2:12: note: did you mean to add a conversion?
error: fatalError
";
        let diags = parse_swift_diagnostics(stderr, mirror);
        assert_eq!(diags.len(), 1, "only the single `error:` line with a location gates: {diags:?}");
        assert_eq!(diags[0].file, "Sources/App/main.swift", "path relativized against the mirror root");
        assert_eq!(diags[0].line, 2);
        assert!(diags[0].message.contains("cannot convert return expression"), "message carried: {}", diags[0].message);

        // A clean build (no error lines) yields nothing.
        assert!(parse_swift_diagnostics("Compiling App\nBuild complete!\n", mirror).is_empty());
        assert!(parse_swift_diagnostics("", mirror).is_empty());
    }

    // The SAME diagnostic emitted once per build phase (emit-module + compile) must collapse to a
    // single instance — otherwise the spine's count-based baseline diff false-rejects a
    // pre-existing error whose phase-multiplicity differs between the baseline and after builds.
    #[test]
    fn swift_diagnostics_are_deduped_across_build_phases() {
        let mirror = Path::new("/tmp/mir");
        let repeated = "\
/tmp/mir/Sources/App/x.swift:2:5: error: cannot find 'foo' in scope
/tmp/mir/Sources/App/x.swift:2:5: error: cannot find 'foo' in scope
/tmp/mir/Sources/App/x.swift:2:5: error: cannot find 'foo' in scope
";
        let diags = parse_swift_diagnostics(repeated, mirror);
        assert_eq!(diags.len(), 1, "three phase-repeats of one error collapse to one: {diags:?}");
    }

    // The head splitter: `PATH:LINE:COL: severity: msg` decomposes correctly, and a
    // `swift build`-style leading `error:` with no location (a summary line) is rejected.
    #[test]
    fn split_diag_head_extracts_path_line() {
        let (p, rest, line) = split_diag_head("/a/b/File.swift:12:5: error: boom").unwrap();
        assert_eq!((p.as_str(), rest, line), ("/a/b/File.swift", "error: boom", 12));
        assert!(split_diag_head("error: no such module 'X'").is_none(), "no location head -> skipped");
        assert!(split_diag_head("Build complete!").is_none());

        // A path that itself contains `: ` must NOT drop the diagnostic (anchor on `:LINE:COL:`).
        let (p, rest, line) = split_diag_head("/tmp/od d: 1/File.swift:9:2: error: nope").unwrap();
        assert_eq!((p.as_str(), rest, line), ("/tmp/od d: 1/File.swift", "error: nope", 9));
    }
}
