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
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where a toolchain process runs. Implementations must be cheap to share across the engines that
/// hold one (`Send + Sync`); a resident backend keeps its own warm state internally.
pub trait Sandbox: Send + Sync {
    /// Whether the toolchain runs in a container rather than on the host. Engines use this to
    /// resolve tools FROM THE IMAGE — a bare command name looked up on the container's PATH, with
    /// no host probe and no host-specific env — instead of a host absolute path. Default: host.
    fn containerized(&self) -> bool {
        false
    }

    /// Run a one-shot command to completion, capturing stdout/stderr capped at `cap` bytes and
    /// killing it after `timeout` — the gate-verdict path. The host impl is [`crate::run_capped`];
    /// a container impl runs the same argv inside its rootfs and maps the output back.
    fn run_capped(&self, cmd: &mut Command, timeout: Duration, cap: usize) -> io::Result<CappedOutput>;

    /// Spawn a long-lived process (a resident LSP / gate sidecar) with the stdio the caller has
    /// already configured on `cmd`, returning the [`Child`]. The host impl is `Command::spawn`; a
    /// container impl execs the argv inside the running container with the same pipes.
    fn spawn(&self, cmd: &mut Command) -> io::Result<Child>;

    // Deliberately NO uncapped/untimed variant: every one-shot gate command goes through
    // `run_capped` (usually via `run_gate_capped`), so an unbounded gate is unrepresentable.
    // The old `output` escape hatch existed for `cargo check`'s large JSON stream — the cap
    // (GATE_OUTPUT_CAP) gives that stream orders-of-magnitude headroom, and truncation can only
    // under-report on an already-failing exit, which `silent_tool_failure_diag` still rejects.
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
}

/// The sandbox a provider should run its toolchain in, for `root`. **This is the single switch
/// for container mode:** every gate engine resolves its sandbox here, so no engine is edited to
/// turn containers on. Opt-in via `$CI_SANDBOX=oci` AND an OCI runtime on PATH; anything else — the
/// default, and everywhere without a runtime — stays on the host, behavior-identical to M1.
pub fn resolve_sandbox(root: &Path, image: &str) -> Arc<dyn Sandbox> {
    if std::env::var("CI_SANDBOX").ok().as_deref() == Some("oci") {
        match oci_runtime() {
            Some(runtime) => {
                return Arc::new(OciSandbox::new(root.to_path_buf(), runtime, image.to_string()))
            }
            None => eprintln!(
                "[ci-core] CI_SANDBOX=oci but no OCI runtime (container/docker/podman/nerdctl) \
                 found on PATH — running the toolchain on the host"
            ),
        }
    }
    Arc::new(HostSandbox)
}

/// Resolve a tool's launch command. Inside a container it is the bare `name` (the image provides
/// it on PATH, with the image's own environment); on the host it is the caller's own resolution —
/// a probe plus any host-specific env (`JAVA_HOME`, a PHAR launcher, …). THE single place the
/// host-vs-container choice lives, so an engine calls this instead of branching on
/// `containerized()` — adding a language is then "declare the tool name + host resolver," not a new
/// `if` in every launcher (docs/container-gate-spec.md §9b).
pub fn tool_command(
    sandbox: &dyn Sandbox,
    name: &str,
    host: impl FnOnce() -> crate::Result<Command>,
) -> crate::Result<Command> {
    if sandbox.containerized() {
        Ok(Command::new(name))
    } else {
        host()
    }
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
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let direct = Path::new(name);
    if direct.is_absolute() {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|c| c.is_file())
    })
}

/// Runs a toolchain inside a warm OCI container via a runtime CLI (Apple `container`, docker,
/// podman, …) so a device needs the runtime instead of every language's toolchain. One detached
/// container is started lazily per sandbox and reused, with the repo AND the system temp dir
/// bind-mounted at their SAME host paths — so `current_dir`, java's `-sourcepath`, the sidecar's
/// materialized-source tempdir, and phpstan's overlay tree all resolve inside the container
/// unchanged (§9b's identical-path trick). Reachable only when `$CI_SANDBOX=oci` and a runtime is
/// present.
pub struct OciSandbox {
    root: PathBuf,
    runtime: PathBuf,
    image: String,
    /// The warm container's id — started on the first op, reused after, killed on drop. `&self`
    /// trait methods need interior mutability; the container is per-sandbox state, so a Mutex fits.
    container: Mutex<Option<String>>,
}

impl OciSandbox {
    pub fn new(root: PathBuf, runtime: PathBuf, image: String) -> Self {
        Self { root, runtime, image, container: Mutex::new(None) }
    }

    /// The warm container's id, starting it on first use: a detached container idling on
    /// `sleep infinity` with the repo + system temp dir mounted at their host paths. `--rm` clears
    /// it once [`Drop`] kills it.
    fn container(&self) -> io::Result<String> {
        let mut guard = self.container.lock().unwrap();
        if let Some(id) = guard.as_deref() {
            return Ok(id.to_string());
        }
        let tmp = std::env::temp_dir();
        let out = Command::new(&self.runtime)
            .args(["run", "-d", "--rm"])
            .arg("-v")
            .arg(same_path_mount(&self.root))
            .arg("-v")
            .arg(same_path_mount(&tmp))
            .arg(&self.image)
            .args(["sleep", "infinity"])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "{} run ({}) failed: {}",
                self.runtime.display(),
                self.image,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        *guard = Some(id.clone());
        Ok(id)
    }

    /// Rewrite a host `Command` as `<runtime> exec [-i] [--workdir CWD] <container> <program>
    /// <args…>`. Host paths are valid in the container (identical-path mounts), so program, args,
    /// and cwd carry over verbatim; the image supplies the environment (host env such as
    /// `JAVA_HOME` would name host-only paths, so it is deliberately NOT forwarded).
    fn exec(&self, container: &str, cmd: &Command, interactive: bool) -> Command {
        let mut d = Command::new(&self.runtime);
        d.arg("exec");
        if interactive {
            d.arg("-i");
        }
        if let Some(cwd) = cmd.get_current_dir() {
            d.arg("--workdir").arg(cwd);
        }
        d.arg(container).arg(cmd.get_program()).args(cmd.get_args());
        d
    }
}

/// A bind-mount spec that maps a host path to the SAME path inside the container (`/x:/x`).
fn same_path_mount(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.trim_end_matches('/');
    format!("{s}:{s}")
}

impl Sandbox for OciSandbox {
    fn containerized(&self) -> bool {
        true
    }

    fn run_capped(&self, cmd: &mut Command, timeout: Duration, cap: usize) -> io::Result<CappedOutput> {
        let container = self.container()?;
        crate::run_capped(&mut self.exec(&container, cmd, false), timeout, cap)
    }

    fn spawn(&self, cmd: &mut Command) -> io::Result<Child> {
        let container = self.container()?;
        // Resident-process stdio convention (LSP / gate sidecar): the caller drives stdin/stdout,
        // stderr dropped — `-i` keeps stdin open through `exec` so the JSON-RPC channel works.
        self.exec(&container, cmd, true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    }
}

impl Drop for OciSandbox {
    fn drop(&mut self) {
        if let Some(id) = self.container.get_mut().unwrap().take() {
            let _ = Command::new(&self.runtime).arg("kill").arg(id).output();
        }
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
    fn find_on_path_resolves_absolute_and_bare_and_rejects_missing() {
        assert_eq!(find_on_path("/bin/sh").as_deref(), Some(Path::new("/bin/sh")));
        assert!(find_on_path("sh").is_some(), "a bare name resolves via PATH");
        assert!(find_on_path("marksman-no-such-binary-xyz").is_none());
        assert!(find_on_path("/nonexistent/abs/path").is_none());
    }

    #[test]
    fn oci_sandbox_errors_when_the_runtime_is_missing() {
        // A bogus runtime can't start the container, so ops error loudly — never a silent host
        // fallback (that would change the verdict environment mid-session).
        let s = OciSandbox::new(
            PathBuf::from("/repo"),
            PathBuf::from("/nonexistent/oci-runtime"),
            "marksman-java".into(),
        );
        let mut cmd = Command::new("true");
        assert!(s.run_capped(&mut cmd, Duration::from_secs(5), 4096).is_err());
    }

    #[test]
    fn same_path_mount_maps_a_path_to_itself() {
        assert_eq!(same_path_mount(Path::new("/a/b")), "/a/b:/a/b");
        assert_eq!(same_path_mount(Path::new("/a/b/")), "/a/b:/a/b", "trailing slash trimmed");
    }

    // Needs docker (or another OCI runtime) up AND the java image built:
    //   docker build -f docker/marksman-java.Dockerfile -t marksman-java docker/
    // Proves the whole M2.2 path: warm container start, identical-path mount, exec, teardown.
    #[test]
    #[ignore]
    fn oci_sandbox_runs_java_in_the_container() {
        let Some(runtime) = oci_runtime() else {
            eprintln!("SKIP: no OCI runtime (docker/podman/nerdctl/container) on PATH");
            return;
        };
        let sb = OciSandbox::new(std::env::temp_dir(), runtime, "marksman-java".into());
        let mut cmd = Command::new("java");
        cmd.arg("-version");
        let out = sb
            .run_capped(&mut cmd, Duration::from_secs(120), 1024 * 1024)
            .expect("java -version inside the container");
        // `java -version` prints to stderr by convention.
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.is_some_and(|s| s.success()) && !out.timed_out, "java ran in the container: {text}");
        assert!(text.contains("21"), "the image's Java 21: {text}");
    }
}
