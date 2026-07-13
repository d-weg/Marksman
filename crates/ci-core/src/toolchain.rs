//! Toolchain requirements — the layer that tells a user WHAT a language needs, WHETHER it's
//! installed, and HOW to get it, instead of a cryptic spawn error mid-run. The shape is
//! language-blind (this crate never names node or rust-analyzer); each provider crate exposes a
//! `toolchain()` returning its own facts, and the registry builders consult it BEFORE
//! constructing a provider — so a repo without a language never probes (let alone installs)
//! that language's tools.
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// One external tool a language provider depends on, and whether it was found.
#[derive(Debug, Clone)]
pub struct ToolStatus {
    /// Binary name(s) as the user knows them, e.g. `"node (+ npx)"`.
    pub tool: &'static str,
    /// What breaks without it — scoped honestly (some tools gate only the WRITE path).
    pub needed_for: &'static str,
    /// Actionable install instruction (command or URL).
    pub install: &'static str,
    /// The probed version line when present; `None` = missing.
    pub found: Option<String>,
}

/// A language's full toolchain check.
#[derive(Debug, Clone)]
pub struct ToolchainReport {
    pub lang: &'static str,
    pub tools: Vec<ToolStatus>,
}

impl ToolchainReport {
    /// True when every required tool was found.
    pub fn ok(&self) -> bool {
        self.tools.iter().all(|t| t.found.is_some())
    }

    pub fn missing(&self) -> impl Iterator<Item = &ToolStatus> {
        self.tools.iter().filter(|t| t.found.is_none())
    }

    /// The actionable message for everything missing, or `None` when complete. This is the
    /// text a user actually sees — one line per tool: what's absent, what it's for, how to
    /// install it.
    pub fn describe_missing(&self) -> Option<String> {
        let lines: Vec<String> = self
            .missing()
            .map(|t| format!("{} requires {} — needed for {}. Install: {}", self.lang, t.tool, t.needed_for, t.install))
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

/// Probe one tool: run it with `args` (conventionally `--version`) and return the first output
/// line on success. `None` = not runnable (missing from PATH, not executable, wrong arch…).
/// Never inherits stdio — a probe must not pollute an MCP/JSON-RPC stream.
pub fn probe_tool(cmd: &mut Command) -> Option<String> {
    let out = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
    Some(if first.is_empty() { "present".into() } else { first })
}

/// Resolve an external tool binary the conventional way: `$<env>` when it names an existing
/// file, else the first of `names` found on `$PATH`, else the first existing path in
/// `fallbacks`. `None` = not installed (the caller's toolchain report says how to get it).
///
/// Env semantics: a set-but-missing `$<env>` falls through to the PATH scan (matching the
/// historical per-provider lookups this replaces). Providers whose env var is
/// unconditional-trust — `CI_RUST_ANALYZER`, `CI_TSGO`, where an explicitly-set-but-wrong path
/// should fail loudly later instead of silently falling through — deliberately do NOT use this.
pub fn discover_tool(env: &str, names: &[&str], fallbacks: &[&str]) -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var(env) {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    for name in names {
        if let Some(p) = crate::sandbox::find_on_path(name) {
            return Some(p);
        }
    }
    fallbacks.iter().map(std::path::PathBuf::from).find(|p| p.is_file())
}

/// The outcome of [`run_capped`].
pub struct CappedOutput {
    /// The child's exit status, or `None` when the deadline killed it (`timed_out`).
    pub status: Option<ExitStatus>,
    /// stdout, truncated at the byte cap (excess drained then dropped).
    pub stdout: Vec<u8>,
    /// stderr, truncated at the byte cap.
    pub stderr: Vec<u8>,
    /// True when the process was killed for exceeding the timeout.
    pub timed_out: bool,
}

/// Default wall-clock ceiling for a gate subprocess, overridable via `CI_GATE_TIMEOUT_SECS`.
/// Deliberately GENEROUS (10 min) — it must never kill a legitimately slow cold build; its job is
/// only to bound a genuinely hung tool (a looping macro, a toolchain deadlock) that would
/// otherwise hang the edit forever.
pub fn gate_timeout() -> Duration {
    let secs = std::env::var("CI_GATE_TIMEOUT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    Duration::from_secs(secs)
}

/// Byte cap for a gate verdict tool's captured stdout/stderr. 32 MiB — a warm compiler pass
/// emits error messages only, so this is orders of magnitude of headroom; its job is bounding a
/// pathologically chatty tool, not trimming a normal one. Truncation is SOUND for a gate: a
/// capped stream can only DROP diagnostics on an already-failing exit code, and
/// [`silent_tool_failure_diag`] turns "failed with nothing parsed" into a reject — so no drop
/// path reaches a false clean.
pub const GATE_OUTPUT_CAP: usize = 32 * 1024 * 1024;

/// Run a one-shot gate VERDICT tool through `sandbox`: output capped at [`GATE_OUTPUT_CAP`],
/// killed at [`gate_timeout`]. A timeout is [`Error::GateTimeout`](crate::Error::GateTimeout) —
/// the caller MUST propagate it (the edit is refused, disk untouched), never map it into a
/// weaker verdict or a fallback engine. Spawn failures are `Driver` (those MAY have a fallback:
/// "tool absent" is honest degrade territory, "tool hung" never is).
pub fn run_gate_capped(
    sandbox: &dyn crate::Sandbox,
    cmd: &mut Command,
    tool: &str,
) -> crate::Result<CappedOutput> {
    let out = sandbox
        .run_capped(cmd, gate_timeout(), GATE_OUTPUT_CAP)
        .map_err(|e| crate::Error::Driver(format!("{tool} spawn: {e}")))?;
    if out.timed_out {
        return Err(crate::Error::GateTimeout(format!(
            "{tool} exceeded the gate timeout ({}s) — set CI_GATE_TIMEOUT_SECS higher if this \
             project legitimately takes longer",
            gate_timeout().as_secs()
        )));
    }
    Ok(out)
}

/// The reject-on-failed-tool invariant, in one place: a gate tool that exits non-zero having
/// produced ZERO parsed diagnostics died before reporting (segfault, OOM-kill, bad config, no
/// runtime) — and the spine reads an empty diagnostic set as clean-commit. Return the one `Diag`
/// that makes the spine REJECT instead of reading silence as clean. `first_line` extracts the
/// per-tool failure message (stderr first line, a `contains("error:")` scan, …) — the only part
/// that is legitimately per-language.
pub fn silent_tool_failure_diag(
    exited_ok: bool,
    parsed: &[crate::Diag],
    anchor_file: &str,
    first_line: impl FnOnce() -> String,
) -> Option<crate::Diag> {
    if exited_ok || !parsed.is_empty() {
        return None;
    }
    Some(crate::Diag { file: anchor_file.into(), code: 0, message: first_line(), line: 0 })
}

/// Run `cmd` capturing stdout/stderr, each capped at `cap` bytes (excess is drained but dropped, so
/// a pathologically chatty tool can't OOM us — B3), and killed after `timeout` (so a hung tool
/// can't hang the edit forever — B4). Two reader threads drain the pipes CONCURRENTLY so a full
/// pipe never deadlocks the child while we wait. `status` is `None` + `timed_out` true on kill.
/// stdin is closed. Prefer this over `Command::output()` for any project-controlled gate tool.
pub fn run_capped(cmd: &mut Command, timeout: Duration, cap: usize) -> std::io::Result<CappedOutput> {
    let mut child =
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let out_pipe = child.stdout.take().expect("piped stdout");
    let err_pipe = child.stderr.take().expect("piped stderr");
    let out_h = drain_capped(out_pipe, cap);
    let err_h = drain_capped(err_pipe, cap);

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(s) => break Some(s),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // The child has exited (or been killed), so both pipes are closed — the readers see EOF and
    // finish; join to collect what they captured.
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(CappedOutput { status, stdout, stderr, timed_out })
}

/// Read `r` to EOF on its own thread, keeping the first `cap` bytes and DRAINING the rest (so the
/// child never blocks on a full pipe, yet our memory stays bounded).
fn drain_capped<R: Read + Send + 'static>(mut r: R, cap: usize) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < cap {
                        let take = n.min(cap - buf.len());
                        buf.extend_from_slice(&chunk[..take]);
                    }
                    // Past the cap: keep reading (to drain the pipe) but drop the bytes.
                }
            }
        }
        buf
    })
}

#[cfg(test)]
mod proc_tests {
    use super::*;

    #[test]
    fn run_capped_truncates_and_times_out() {
        // Cap: a burst far larger than the cap is truncated, and draining doesn't hang.
        let mut c = Command::new("sh");
        c.args(["-c", "yes ABCDEFGH | head -c 200000"]);
        let r = run_capped(&mut c, Duration::from_secs(30), 1000).unwrap();
        assert_eq!(r.stdout.len(), 1000, "stdout truncated to the cap");
        assert!(!r.timed_out && r.status.map(|s| s.success()).unwrap_or(false));

        // Timeout: a sleeper past the deadline is killed, not waited on forever.
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30"]);
        let r = run_capped(&mut c, Duration::from_millis(200), 4096).unwrap();
        assert!(r.timed_out && r.status.is_none(), "the hung child was killed at the deadline");
    }
}
