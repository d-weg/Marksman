# Container mode — a guide

Peashooter's gate (the type-check verdict) and rename engine need each language's real
toolchain. **Container mode** runs that toolchain from a per-language OCI image at pinned
versions, so a host needs a **container runtime instead of N language toolchains** — and the
verdict is pinned to the image, so a host upgrade can never silently change what the gate
accepts. This guide walks the whole thing; the design and measurements are in
[container-gate-spec.md](container-gate-spec.md), the image details in
[../docker/README.md](../docker/README.md).

**Platform note:** container mode is Linux and macOS (with a Linux container runtime).
Without a runtime, Peashooter runs the toolchain on the host (`HostSandbox`) exactly as before —
container mode is opt-in and additive.

## The helper

Everything below is driven by one script — [`scripts/peashooter-images.sh`](../scripts/peashooter-images.sh):

```
scripts/peashooter-images.sh check          # runtime present? which images exist? are the pins in sync?
scripts/peashooter-images.sh build          # build every image
scripts/peashooter-images.sh build ts rust  # build only what your repos need
scripts/peashooter-images.sh list           # what each image holds, and its size
```

## Step 1 — a container runtime

Install one of `container` (Apple's), `docker`, `podman`, or `nerdctl`. Peashooter picks the
first on `PATH`; `CI_SANDBOX_RUNTIME` names one explicitly (bare name or absolute path). The
runtime choice never changes a verdict — the images are plain OCI.

```
scripts/peashooter-images.sh check
# runtime:   docker
```

If it says `runtime: NONE`, install a runtime — with `CI_SANDBOX=oci` and no runtime,
Peashooter warns at startup and stays on the host (loud, never silent).

## Step 2 — build the images you need

Build lazily-relevant images only — a language's container starts on its first operation and
only when that language's files exist in the repo. For a TypeScript + Rust repo:

```
scripts/peashooter-images.sh build ts rust
```

Images are **built locally, never pulled** — the toolchain is auditable, and nothing reaches
out to a registry. Sizes range from small (`rust`) to large (`swift`, ~2.5GB — the compiler
is the size); `list` shows them.

## Step 3 — enable it

```
CI_SANDBOX=oci peashooter-mcp --root /path/to/repo
```

or in an MCP client config:

```json
{ "command": "peashooter-mcp", "args": ["--root", "."], "env": { "CI_SANDBOX": "oci" } }
```

That's it. Peashooter starts one warm container per language on demand, reuses it across
operations, and removes it on exit. The repo and the system temp dir are bind-mounted at
their **identical host paths**, so every path in requests, replies, and `.peashooter/`
artifacts is valid on both sides.

## Verifying / troubleshooting

`scripts/peashooter-images.sh check` is the one-stop status:

- **`missing peashooter-<lang>`** — build it; until then, an operation on that language errors
  loudly at first use (no silent fallback to the host toolchain — that would change the
  verdict environment, which the [provider contract §9](provider-contract.md) forbids).
- **`PIN DRIFT`** — the `ts` image's `scip-typescript` / `typescript` / `tsgo` versions must
  match `crates/langs/lang-ts/src/engine.rs`. The pin-in-image is the whole point (contract
  §10): a drifted pin means the image would reproduce a different verdict than the code
  expects. `build ts` refuses to proceed while drifted — bump the Dockerfile and the code
  constant together, deliberately.
- **`runtime: NONE`** — see Step 1.

## What the images hold, and why TS is special

| image | toolchain | role |
|---|---|---|
| `peashooter-rust` | cargo + rust-analyzer | gate + rename |
| `peashooter-ts` | scip-typescript + tsgo + typescript | gate + rename **+ indexer** |
| `peashooter-java` | JDK 21 + jdtls | gate + rename |
| `peashooter-php` | php + phpstan + phpactor | gate + rename |
| `peashooter-swift` | the Swift toolchain | gate + rename |

TypeScript is the one language whose **read** path (the SCIP indexer) also needs the
toolchain, so `peashooter-ts` carries `scip-typescript` in addition to the gate/rename tier —
which is why its three pins are the ones the helper checks.
