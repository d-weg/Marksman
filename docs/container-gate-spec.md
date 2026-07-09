# Containerized gate — spec for review

**Status: DRAFT (2026-07-09, branch `container-gate`).** Motivated by the java/php/swift
bench review ([the lang-suite run](benchmarks.md) surfaced that the worst cells were toolchain
*availability*, not logic: java rename fell back to fully manual editing because jdtls was
absent). The idea: run a language's gate/rename toolchain inside a sandboxed root filesystem, so
a device running Marksman needs a container runtime instead of N language toolchains, and the
gate's verdict is pinned to a known toolchain version. This document fixes the goal, picks the
mechanism, names the seam in the current code, and phases the work with acceptance criteria.
Nothing here is implemented yet.

## Terms

- **Gate** — the pre-commit type-check: an edit is checked with every file it could break; a new
  error means nothing is written (provider contract §5). Each language backs it with a real
  tool: `javac` (a resident sidecar), `phpstan`, `swift build`, `cargo check`.
- **Rename engine** — the cross-file reference rewriter: an LSP (`jdtls`, `phpactor`,
  `sourcekit-lsp`, `rust-analyzer`) when present; the syntactic movefix hooks for *moves* when
  absent. Symbol *rename* has no syntactic fallback today — it hard-requires the LSP.
- **Overlay** — the uncommitted edit content shadowing disk, assembled by the VFS. The gate runs
  against a *materialized* overlay (the coherent project the tool sees), never against disk.
- **Toolchain** — the per-language binaries the above need on the host: `javac`+`jdtls`,
  `php`+`phpstan`+`phpactor`, the Swift toolchain, `cargo`+`rust-analyzer`, Node+`npx`.
- **Rootfs image** — a filesystem tree (OCI image or a plain tarball) containing a toolchain,
  runnable in isolation. This is the thing that removes host installs.
- **OCI** — the Open Container Initiative image+runtime spec. Building against it means *any*
  compliant runtime (runc, crun, youki, containerd, podman) can run the image — no vendor lock.

## 1. Goal, and the split that decides the mechanism

Two different wishes get bundled under "sandbox," and they need **different** tools. Naming the
one we're actually after is the whole design decision:

| Wish | What it requires | Mechanism |
|---|---|---|
| **A — no toolchain installs on the device; pinned, reproducible verdicts** *(our goal)* | a **rootfs image** carrying the toolchain | container / VM (OCI runtime) |
| B — run untrusted project code safely | restrict what a *host* process may touch | process sandbox (`landlock`/`seccomp`) |

The trap: the thing that is *easy to build in Rust* — a `landlock`+`seccomp` process sandbox
(`sandlock`, `sandbox-rs`) — is wish **B**. It restricts a process but ships no toolchain, so
the host still needs `javac`/`phpstan`/etc. installed. **It does not achieve our goal.** Wish A
needs a rootfs, which means an OCI runtime (or a VM). We optimize for A; the isolation B provides
comes along for free but is not the requirement.

**Secondary win, arguably the bigger one:** a gate's verdict is only trustworthy if it's
reproducible. Today the verdict depends on whatever `phpstan`/`javac` version the host happens to
have. A pinned image fixes the toolchain version → the same edit gates identically on every
device and in CI. For a tool whose entire pitch is "trust the gate, don't re-verify by hand,"
that is load-bearing.

### Non-goals
- **Not security-first.** We run analyzers we mostly trust; isolation is a bonus, not the driver.
- **Not Docker-specific.** We target the OCI spec and a daemonless runtime; Docker Desktop's
  licensing/daemon is exactly what we avoid.
- **Not macOS-native.** See §6 — the sandbox is a Linux capability; macOS dev keeps the host path.
- **Not mandatory.** Container mode is opt-in per language/deployment; a host with the toolchain
  already installed keeps the fast in-process path unchanged.

## 2. Mechanism decision

**Primary: an OCI rootfs run by a daemonless, open runtime — `youki`'s `libcontainer` crate
(in-process, Rust) as the reference backend.** `libcontainer` (crates.io, part of the youki OCI
runtime) lets us create OCI containers from inside the Rust process — namespaces + cgroups +
rootfs — with no daemon and no Docker. It is the maximally-open, Rust-native end of the spectrum.

Kept behind a **`Sandbox` trait** (§3) so the backend is swappable without touching any engine:
- `libcontainer` in-process (reference; no external runtime),
- shell out to a single-binary OCI runtime (`crun`/`youki`/`runc`) against an OCI bundle,
- shell out to `podman`/`nerdctl` (for users who already run one),
- `HostSandbox` — no isolation, runs on the host (today's behavior; the macOS/dev path).

**Rejected for our goal:**
- *Docker Engine/Desktop* — daemon dependency + Desktop licensing; nothing it offers that a
  daemonless OCI runtime doesn't. (Its images are still OCI, so a user *may* back the trait with
  it.)
- *`landlock`/`seccomp` process sandboxes* — wish B, not A: no rootfs, host still needs the
  toolchain. Reconsider only if "sandbox untrusted repos" becomes a first-class goal.
- *microVM (Firecracker)* — own-kernel isolation, Linux+KVM only; overkill for a type-checker.
- *WASM (wasmtime)* — cross-platform and strongly sandboxed, but the toolchains (JVM, php,
  swiftc) don't compile to WASM. Non-starter for running real compilers.

## 3. The seam in the current code

Process spawning already funnels through **two chokepoints** — the entire integration surface:

1. **One-shot gate commands** — `Command::new(tool)` → `ci_core::run_capped(cmd, timeout, cap)`
   (php `gate.rs`, swift `gate.rs`, rust `gate.rs`). Produces diagnostics from stdout.
2. **Resident processes** — `LspClient::start(root, cmd)` (jdtls, phpactor, sourcekit) and the
   java `JavacSidecar` (`Command::new("java")`). JSON-line / LSP over stdio.

Both take a `Command`. So the seam is a `Sandbox` that owns *where a `Command` runs and where its
overlay lives*:

```
trait Sandbox {
    // one-shot: materialize `overlay` inside the sandbox, run argv, return captured output
    fn run(&self, overlay: &[(String,String)], argv: &Command, timeout, cap) -> Result<Output>;
    // resident: spawn a long-lived process inside the sandbox, return a stdio channel
    fn spawn(&self, argv: &Command) -> Result<Child>;
}
```

`ci_core::run_capped` and `LspClient::start` each gain a `Sandbox`-aware path; `HostSandbox`
reproduces exactly today's code, so wiring it in is a **behavior-preserving refactor** (M1) before
any container exists.

### A nuance that shrinks the hard part

The costly bit of containerizing is getting the **overlay filesystem** across the boundary. But
only some gates need a filesystem:

- **Buffer/stdio gates cross for free.** The java `JavacSidecar` takes overlay buffers over
  stdin; every LSP speaks over stdio. Running these *inside* the container is just stdio
  redirection — **no mount, no copy.** java's whole gate and all four rename engines fall here.
- **Tree gates need the overlay mounted/copied in.** `phpstan`, `swift build`, `cargo check`
  read a materialized directory (my recent php fix mirrors the whole project). These are where
  mount I/O (§7) bites.

So the migration order writes itself: the buffer/stdio engines (java, the LSPs) containerize
cheaply and first; the tree gates (php/swift/rust) come after the I/O approach is measured.

### Path mapping
LSP/diagnostic messages carry container paths (e.g. `/work/src/Foo.php`); the provider speaks
repo-relative. The gates already relativize against a temp-tree prefix (php `parse_phpstan_json`,
the java sidecar's `root_prefix`) — the container mount point is just a different, fixed prefix.
Small extension, not new machinery.

## 4. Warm-container lifecycle

The gate is on the hot edit path; cold-vs-warm is 100×+ (the tsgo measurement). Therefore:

- **One resident container per repo/session, started lazily on first gated op** — mirrors how
  jdtls already persists its eclipse workspace per repo. Never a container per gate.
- The **resident toolchain processes (jdtls/phpactor/sourcekit, the java sidecar) live inside**
  the container and stay warm across edits; the host holds the stdio channel.
- Teardown on session end; a stable per-repo name allows warm reuse across sessions.
- Failure policy is the contract's: a sandbox that won't start is an actionable "language
  disabled" (like a missing toolchain today), never a silent host fallback that would change the
  verdict environment mid-session.

## 5. The image

- **One image per language** (or a composed multi-lang image) with the toolchain pinned to exact
  versions — that pin *is* the reproducible-verdict guarantee.
- Built from the OCI spec (buildable by `buildah`/`podman build`/`docker build` — any producer).
- Size is real (a JVM+jdtls image is ~hundreds of MB; swift larger). Ship per-language so a
  php-only user never pulls the swift image; lazy-pull on first use, like every other toolchain.
- `marksman doctor` learns a container tier: reports the runtime, the image (present/pullable),
  and pinned versions — the same actionable shape it uses for host toolchains.

## 6. Platform reality (read before estimating)

Namespaces/cgroups/`libcontainer`/landlock are **Linux-only**. On macOS none run natively — a
container always means a Linux VM underneath (what Docker Desktop/podman-machine/colima do). So:

- **Dev on macOS uses `HostSandbox`** (host toolchains, today's path). The container tier is a
  **Linux-host feature** (servers, CI, Linux workstations), or macOS-with-a-VM if a user opts in.
- End-to-end testing of the OCI path therefore needs a Linux environment (§8). The
  behavior-preserving `HostSandbox` refactor (M1) is fully testable on macOS.

This is the one place the idea is genuinely constrained: it does **not** make Marksman
install-free on a Mac. It makes it install-free on Linux, and version-reproducible everywhere it
runs.

## 7. Risks to measure before committing past M2

- **Overlay I/O across the boundary — the make-or-break number.** Bind mounts on the macOS VM are
  notoriously slow, and the php gate re-mirrors the whole project per call. Measure copy-in vs
  mount vs named-volume for the tree gates on a large repo; if it erases the gate's speed
  advantage, keep tree gates on the host and containerize only the buffer/stdio engines (still
  solves the jdtls/phpactor-absent problem, which was the actual pain).
- **Cold start** of the resident container on first gated op (amortized by warm reuse, but the
  first edit pays it).
- **Image size / pull time** on first use.
- **Path-mapping correctness** across the boundary (a regression here = wrong-file diagnostics).

## 8. Testing plan

- **`HostSandbox` (M1):** unit + the existing `#[ignore]` gate e2e, unchanged, on any platform —
  proves the refactor is behavior-preserving.
- **OCI path (M2+):** needs Linux. Run the same `#[ignore]` gate batteries with
  `CI_SANDBOX=oci` on a Linux CI job / VM / remote box. Acceptance = byte-identical verdicts to
  the host path on the fixtures, plus the java/php rename cells now succeed *without* a host
  jdtls/phpactor (the toolchain comes from the image).

## 9. Phasing (each independently acceptable)

- **M0 — this spec.** Decision on the mechanism (§2) and whether to pursue at all.
- **M1 — `Sandbox` trait + `HostSandbox`, behavior-preserving.** Route `run_capped` and
  `LspClient::start` through it. Acceptance: zero-warning, full suite + all gate e2e green,
  verdicts unchanged. Testable on macOS. *No container yet.*
- **M2 — `OciSandbox` (libcontainer) for the buffer/stdio engines** (java sidecar + the four
  LSPs). Acceptance: on Linux, java gate + a rename work with **no host jdtls** installed; verdicts
  identical. This is the piece that directly closes the bench's worst finding.
- **M3 — tree gates in the container** (php/phpstan, swift build, cargo check), *after* §7's I/O
  measurement picks the transport. Acceptance: identical verdicts; gate latency within an agreed
  budget of the host path.
- **M4 — image build + `doctor` integration + docs.** Per-language pinned images, lazy pull,
  `marksman doctor` container tier, a one-line opt-in (`CI_SANDBOX=oci`).

## 10. Relationship to prior findings

- **Supersedes the install burden.** The README simplification made the *core* install trivial,
  but per-language toolchains (jdtls, phpactor, the Swift toolchain) remain the real friction.
  M2/M4 remove them on Linux by shipping them in an image.
- **An alternative to the gated-syntactic-rename proposal.** The bench review offered a
  compiler-gated syntactic rename to remove the hard jdtls/phpactor dependency. The container
  reaches the same "rename works on any machine" outcome by *shipping the precise LSP* instead of
  approximating it. They're not exclusive — syntactic rename helps macOS/host mode where the
  container can't run; the image gives semantic precision on Linux. Pick per the M2 result.
