//! The execution boundary — where a language's gate/rename toolchain actually runs.
//!
//! Every provider spawns its toolchain through one of two shapes: a one-shot capped command
//! (`phpstan`, `swift build`, `cargo check` → [`run_capped`](crate::run_capped)) or a resident
//! process it talks to over stdio (an LSP, the java gate sidecar → `Command::spawn`). A
//! [`Sandbox`] owns *where* those run. The only implementation today is [`HostSandbox`] — run on
//! this machine, exactly as before — so introducing the trait changes no behavior. A future
//! container backend (see `docs/container-gate-spec.md`) implements the same two methods against
//! an OCI rootfs, so a device needs a container runtime instead of every language's toolchain.
use crate::CappedOutput;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::Arc;
use std::time::Duration;

/// Where a toolchain process runs. Implementations must be cheap to share across the engines that
/// hold one (`Send + Sync`); a resident backend keeps its own warm state internally.
pub trait Sandbox: Send + Sync {
    /// Run a one-shot command to completion, capturing stdout/stderr capped at `cap` bytes and
    /// killing it after `timeout` — the gate-verdict path. The host impl is [`crate::run_capped`];
    /// a container impl runs the same argv inside its rootfs and maps the output back.
    fn run_capped(&self, cmd: &mut Command, timeout: Duration, cap: usize) -> io::Result<CappedOutput>;

    /// Spawn a long-lived process (a resident LSP / gate sidecar) with the stdio the caller has
    /// already configured on `cmd`, returning the [`Child`]. The host impl is `Command::spawn`; a
    /// container impl execs the argv inside the running container with the same pipes.
    fn spawn(&self, cmd: &mut Command) -> io::Result<Child>;

    /// Run a one-shot command to completion, returning its full output UNCAPPED and UNTIMED — for a
    /// gate whose output must not be truncated (`cargo check --message-format=json` emits one JSON
    /// object per diagnostic and can legitimately be large). The host impl is `Command::output`.
    fn output(&self, cmd: &mut Command) -> io::Result<Output>;
}

/// The default backend: no isolation. Runs every toolchain on the host exactly as the code did
/// before the trait existed — the behavior-preserving path, and the only one on a platform
/// without OCI (macOS runs this; the container backend is a Linux capability).
#[derive(Clone, Copy, Default, Debug)]
pub struct HostSandbox;

impl Sandbox for HostSandbox {
    fn run_capped(&self, cmd: &mut Command, timeout: Duration, cap: usize) -> io::Result<CappedOutput> {
        crate::run_capped(cmd, timeout, cap)
    }

    fn spawn(&self, cmd: &mut Command) -> io::Result<Child> {
        cmd.spawn()
    }

    fn output(&self, cmd: &mut Command) -> io::Result<Output> {
        cmd.output()
    }
}

/// The sandbox a provider should run its toolchain in, for `root`. **This is the single switch
/// for container mode:** every gate engine resolves its sandbox here, so no engine is edited to
/// turn containers on. Opt-in via `$CI_SANDBOX=oci` AND an OCI runtime on PATH; anything else — the
/// default, and everywhere without a runtime — stays on the host, behavior-identical to M1.
pub fn resolve_sandbox(root: &Path) -> Arc<dyn Sandbox> {
    if std::env::var("CI_SANDBOX").ok().as_deref() == Some("oci") {
        match oci_runtime() {
            Some(runtime) => return Arc::new(OciSandbox::new(root.to_path_buf(), runtime)),
            None => eprintln!(
                "[ci-core] CI_SANDBOX=oci but no OCI runtime (container/docker/podman/nerdctl) \
                 found on PATH — running the toolchain on the host"
            ),
        }
    }
    Arc::new(HostSandbox)
}

/// The OCI runtime CLI an [`OciSandbox`] drives: `$CI_SANDBOX_RUNTIME` (a bare name or an absolute
/// path), else the first found on `PATH` of `container` (Apple's native macOS runtime — a
/// per-container VM on Virtualization.framework, no daemon), `docker`, `podman`, `nerdctl`. `None`
/// = none installed, so [`resolve_sandbox`] stays on the host. The image is plain OCI, so the
/// choice of runtime never changes the verdict — only where the toolchain runs.
pub fn oci_runtime() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CI_SANDBOX_RUNTIME") {
        return find_on_path(&explicit);
    }
    ["container", "docker", "podman", "nerdctl"].iter().find_map(|n| find_on_path(n))
}

/// Resolve `name` — an absolute path or a bare command — to an existing executable, honoring
/// `$PATH` for the bare form. Read-only (never mutates the environment).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let direct = Path::new(name);
    if direct.is_absolute() {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|c| c.is_file())
    })
}

/// Runs a toolchain inside a warm OCI container via a runtime CLI (Apple `container`, docker,
/// podman, …) so a device needs the runtime instead of every language's toolchain. The
/// warm-container start / exec / teardown and the identical-path mounts land in **M2.2** (see
/// `docs/container-gate-spec.md` §9b) — this skeleton carries the `runtime` + `root` that exec
/// will mount and target. Reachable only when `$CI_SANDBOX=oci` and a runtime is present, so the
/// Unsupported errors below never fire on the default host path.
pub struct OciSandbox {
    root: PathBuf,
    runtime: PathBuf,
}

impl OciSandbox {
    pub fn new(root: PathBuf, runtime: PathBuf) -> Self {
        Self { root, runtime }
    }

    /// Until M2.2 wires the container exec, every op is honestly Unsupported (never a silent
    /// host fallback — that would change the verdict environment mid-session).
    fn unsupported(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "OciSandbox (runtime {}, root {}) — container exec is wired in M2.2",
                self.runtime.display(),
                self.root.display()
            ),
        )
    }
}

impl Sandbox for OciSandbox {
    fn run_capped(&self, _cmd: &mut Command, _timeout: Duration, _cap: usize) -> io::Result<CappedOutput> {
        Err(self.unsupported())
    }

    fn spawn(&self, _cmd: &mut Command) -> io::Result<Child> {
        Err(self.unsupported())
    }

    fn output(&self, _cmd: &mut Command) -> io::Result<Output> {
        Err(self.unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_sandbox_run_capped_matches_the_free_fn() {
        let mut cmd = Command::new("printf");
        cmd.arg("hello");
        let out = HostSandbox.run_capped(&mut cmd, Duration::from_secs(5), 4096).unwrap();
        assert!(out.status.is_some_and(|s| s.success()) && !out.timed_out);
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn host_sandbox_spawn_runs_on_the_host() {
        let mut cmd = Command::new("true");
        let mut child = HostSandbox.spawn(&mut cmd).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn host_sandbox_output_returns_full_uncapped_output() {
        let mut cmd = Command::new("printf");
        cmd.arg("diag");
        let out = HostSandbox.output(&mut cmd).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"diag");
    }

    #[test]
    fn find_on_path_resolves_absolute_and_bare_and_rejects_missing() {
        assert_eq!(find_on_path("/bin/sh").as_deref(), Some(Path::new("/bin/sh")));
        assert!(find_on_path("sh").is_some(), "a bare name resolves via PATH");
        assert!(find_on_path("marksman-no-such-binary-xyz").is_none());
        assert!(find_on_path("/nonexistent/abs/path").is_none());
    }

    #[test]
    fn oci_sandbox_skeleton_is_unsupported_until_m2_2() {
        // The skeleton never silently falls back to the host — it errors loudly (M2.2 fills it in).
        let s = OciSandbox::new(PathBuf::from("/repo"), PathBuf::from("/usr/bin/container"));
        let mut cmd = Command::new("true");
        assert_eq!(s.spawn(&mut cmd).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            s.output(&mut cmd).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
