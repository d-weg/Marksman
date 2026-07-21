# Peashooter language images — container mode

One image per gated language, holding its whole toolchain at **pinned versions**: the gate
(type-check verdict), the rename engine (LSP), and — for TypeScript only — the indexer
(scip-typescript; TS is the one language whose *read* path needs the toolchain). With
`CI_SANDBOX=oci`, a host needs a container runtime instead of the language's toolchain, and
the verdict is pinned to the image — a host upgrade can never silently change what the gate
accepts. Design + measurements: [docs/container-gate-spec.md](../docs/container-gate-spec.md).

## Build (local — images are never pulled)

> The helper `scripts/peashooter-images.sh` wraps all of this with runtime detection and a pin
> check — `check` / `build [langs...]` / `list`. See [docs/container-guide.md](../docs/container-guide.md)
> for the full walkthrough. The manual commands below are the equivalent it runs.

```bash
docker build -f docker/peashooter-ts.Dockerfile    -t peashooter-ts    docker/   # ~515MB — scip-typescript + tsgo + typescript
docker build -f docker/peashooter-rust.Dockerfile  -t peashooter-rust  docker/   #        — cargo + rust-analyzer
docker build -f docker/peashooter-java.Dockerfile  -t peashooter-java  docker/   # ~905MB — JDK 21 + jdtls
docker build -f docker/peashooter-php.Dockerfile   -t peashooter-php   docker/   # ~450MB — php + phpstan + phpactor
docker build -f docker/peashooter-swift.Dockerfile -t peashooter-swift docker/   # ~2.5GB — the Swift toolchain (the compiler is the size)
```

Build only what your repos need — a language's container starts lazily, on its first
operation, and only when that language's files exist in the repo.

## Enable

```bash
CI_SANDBOX=oci peashooter-mcp --root /path/to/repo
# or in an MCP client config:  "env": { "CI_SANDBOX": "oci" }
```

- **Runtime**: the first of `container` (Apple), `docker`, `podman`, `nerdctl` found on PATH;
  `CI_SANDBOX_RUNTIME` names one explicitly (bare name or absolute path). The images are
  plain OCI — the runtime choice never changes a verdict.
- **Failure semantics are loud, never silent**: `CI_SANDBOX=oci` without a runtime warns at
  startup and stays on the host; a configured runtime with a **missing image** errors at the
  first operation. There is no mid-session fallback to a different toolchain — that would
  change the verdict environment silently, which the provider contract forbids (§9).
- **Mechanics**: one warm, detached container per language, reused across operations, removed
  on exit. The repo and the system temp dir are bind-mounted at their identical host paths,
  so every path in requests, replies, and artifacts (`.peashooter/`) is valid on both sides.
  Tools are resolved by bare name on the image's PATH (`ci_core::tool_command`).

## Version pins

Each Dockerfile pins its toolchain; the TS pins **must match**
`crates/langs/lang-ts/src/engine.rs` (`SCIP_TS_VERSION`, `TSGO_VERSION`,
`TYPESCRIPT_VERSION`) — the pin-in-image is the point (provider contract §10): bump the
constant and the Dockerfile together, deliberately.
