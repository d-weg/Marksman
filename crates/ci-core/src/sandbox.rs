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
use std::path::Path;
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
/// for container mode:** every gate engine resolves its sandbox here, so turning on an OCI backend
/// (M2) changes only this function — no engine is edited. M1 always returns the host, so the whole
/// codebase is behavior-identical to before the trait existed.
pub fn resolve_sandbox(_root: &Path) -> Arc<dyn Sandbox> {
    Arc::new(HostSandbox)
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
}
