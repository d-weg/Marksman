//! Engine plumbing for the TypeScript provider: write-engine selection (tsgo → ts-morph →
//! tsls), the LSP command builders, the pinned toolchain versions, and the npm/NPX cache
//! discipline shared by every Node-tooling path in this crate.
use ci_core::{Error, Result};
use ci_edit::GateEngine;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned TS toolchain. Unpinned npx/npm floats to "latest", which drifts under us: a new
/// scip-typescript can change index content between two startups (silently invalidating the
/// cache semantics), and a new tsserver/typescript changes what the gate accepts. Bump these
/// deliberately; `SCIP_TS_VERSION` participates in the source fingerprint, so bumping it
/// reindexes on the next startup.
pub(crate) const SCIP_TS_VERSION: &str = "0.4.0";
const TS_LSP_VERSION: &str = "5.3.0";
const TYPESCRIPT_VERSION: &str = "6.0.3";
/// `@typescript/native-preview` publishes DATED DEV BUILDS on a moving stream — the one TS
/// tool that was still fetched unpinned (producer-surface spec F1). The pin is a dated build;
/// bump deliberately. Local tsgo (`CI_TSGO` / PATH) is unconditional-trust and unaffected.
const TSGO_VERSION: &str = "7.0.0-dev.20260707.2";

/// Fresh npm cache dir so a corrupted default `~/.npm` cache can't break `npx`. Shared with the
/// ts-morph sidecar (`tsmorph.rs`) so both TS tooling paths use the same cache location.
pub(crate) fn npm_cache() -> PathBuf {
    std::env::var("CI_NPM_CACHE").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("ci-npm-cache"))
}

/// A best-effort cross-process advisory lock so concurrent `npx` invocations don't corrupt the
/// SHARED npm cache. `npx --yes` stages packages into `<cache>/_npx/<hash>` with atomic renames;
/// two invocations racing there produce `ENOTEMPTY` / half-installed packages (`Cannot find module
/// './Counter'`), so scip-typescript fails intermittently whenever several MCP instances start at
/// once (an agent benchmark, or a few editor sessions). Held for the npx run, released on drop.
/// Best-effort: a stale lock (crashed holder) is stolen after 5 min, and we give up waiting after
/// 3 min and proceed unlocked rather than ever hang the tool.
pub(crate) struct NpxCacheLock(PathBuf);

impl NpxCacheLock {
    pub(crate) fn acquire() -> Option<Self> {
        let dir = npm_cache();
        let _ = std::fs::create_dir_all(&dir);
        let lock = dir.join(".npx.lock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Some(NpxCacheLock(lock)),
                Err(_) => {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|e| e.as_secs() > 300);
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
}

impl Drop for NpxCacheLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Start the lightest available write engine for `root`: ts-morph in-process (synchronous,
/// no LSP settle race) when its sidecar can start, else the generic LSP server. Override with
/// `CI_EDIT_ENGINE=lsp|tsmorph`.
/// Engine preference: **tsgo → ts-morph → tsls**. tsgo (the TS7 native LSP) gates ~138x
/// faster warm than the alternatives with identical verdicts (docs/benchmarks.md), but is
/// auto-picked only when it needs NO network (`CI_TSGO`, or `tsgo` on PATH) — a surprise npx
/// download doesn't belong in the middle of someone's first edit. `CI_EDIT_ENGINE` forces one
/// tier: `tsgo` | `tsmorph` | `lsp` (tsls, or whatever `CI_TS_LSP_SERVER` names).
pub(crate) fn start_engine(root: &Path) -> Result<Box<dyn GateEngine + Send>> {
    let pref = std::env::var("CI_EDIT_ENGINE").unwrap_or_default();
    match pref.as_str() {
        "tsgo" => return Ok(Box::new(ci_lsp::LspClient::start(root, tsgo_lsp_command())?)),
        "lsp" => return start_tsls(root),
        _ => {}
    }
    if pref.is_empty() {
        if let Some(bin) = local_tsgo() {
            let mut c = Command::new(bin);
            c.args(["--lsp", "-stdio"]);
            match ci_lsp::LspClient::start(root, c) {
                Ok(client) => return Ok(Box::new(client)),
                Err(e) => eprintln!("[lang-ts] local tsgo failed to start ({e}); falling back to ts-morph"),
            }
        }
    }
    match crate::tsmorph::TsMorphClient::start(root) {
        Ok(c) => return Ok(Box::new(c)),
        Err(e) if pref == "tsmorph" => return Err(e), // forced: surface the failure
        Err(_) => {} // auto: fall back to LSP
    }
    start_tsls(root)
}

/// The tsls fallback tier, with the toolchain-aware error message.
fn start_tsls(root: &Path) -> Result<Box<dyn GateEngine + Send>> {
    match ci_lsp::LspClient::start(root, ts_lsp_command()) {
        Ok(c) => Ok(Box::new(c)),
        // Both engines need Node; when the toolchain itself is the problem, say THAT (with the
        // install hint) instead of a raw spawn error.
        Err(e) => match crate::toolchain().describe_missing() {
            Some(missing) => Err(Error::Driver(format!("TypeScript edit engine failed to start ({e}).\n{missing}"))),
            None => Err(e),
        },
    }
}

/// A tsgo binary that costs nothing to use: `CI_TSGO`, else a `tsgo` on PATH. `None` means
/// the auto tier skips tsgo (explicit `CI_EDIT_ENGINE=tsgo` may still fetch via npx).
fn local_tsgo() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CI_TSGO") {
        return Some(PathBuf::from(bin));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|d| ["tsgo", "tsgo.exe", "tsgo.cmd"].map(|n| d.join(n)))
        .find(|p| p.is_file())
}

/// The tsgo (TypeScript 7 native) LSP command for the sweep indexer. `CI_TSGO` points at
/// a tsgo binary directly; the default fetches the native preview via npx (the `typescript`
/// RC npm package ships only `tsc` — the LSP binary lives in `@typescript/native-preview`).
pub(crate) fn tsgo_lsp_command() -> Command {
    if let Ok(bin) = std::env::var("CI_TSGO") {
        let mut c = Command::new(bin);
        c.args(["--lsp", "-stdio"]);
        return c;
    }
    let mut c = Command::new("npx");
    c.arg("--yes")
        .arg("-p")
        .arg(format!("@typescript/native-preview@{TSGO_VERSION}"))
        .args(["tsgo", "--lsp", "-stdio"])
        .env("npm_config_cache", npm_cache());
    c
}

/// The TS language-server command (npx tsls). All external/Node tooling lives
/// here in the provider — the core + ci-lsp stay pure Rust.
///
/// `CI_TS_LSP_SERVER` overrides the whole command line (whitespace-split, no quoting) —
/// e.g. `".../node_modules/.bin/tsgo --lsp -stdio"` runs the TS7 native-port server.
fn ts_lsp_command() -> Command {
    if let Ok(raw) = std::env::var("CI_TS_LSP_SERVER") {
        let mut parts = raw.split_whitespace();
        if let Some(prog) = parts.next() {
            let mut c = Command::new(prog);
            c.args(parts);
            return c;
        }
    }
    let mut c = Command::new("npx");
    c.arg("--yes")
        .arg("-p")
        .arg(format!("typescript-language-server@{TS_LSP_VERSION}"))
        .arg("-p")
        .arg(format!("typescript@{TYPESCRIPT_VERSION}"))
        .args(["typescript-language-server", "--stdio"])
        .env("npm_config_cache", npm_cache());
    c
}
