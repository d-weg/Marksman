# Cutting a release

Releases are prebuilt binaries attached to a GitHub Release. The
[`.github/workflows/release.yml`](../.github/workflows/release.yml) workflow does the work
when you push a semver tag `vX.Y.Z`. No secrets are required — the publish step uses the
built-in `GITHUB_TOKEN` (the workflow grants itself `contents: write`).

## Steps

1. **Bump the version** in `Cargo.toml` under `[workspace.package]` (`version = "X.Y.Z"`), and commit.
2. **Update the changelog** — move the `## [Unreleased]` notes in [`CHANGELOG.md`](../CHANGELOG.md)
   under a new `## [X.Y.Z] - YYYY-MM-DD` heading, and commit.
3. **Tag and push:**
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

The workflow then builds four targets, packages each into
`peashooter-mcp-X.Y.Z-<target>.tar.gz` (containing `peashooter`, `peashooter-mcp`, `README.md`,
`LICENSE`) with a matching `.sha256`, and publishes a GitHub Release with auto-generated notes
and a combined `SHA256SUMS`. A tag containing a hyphen (e.g. `v0.2.0-rc1`) is marked as a
pre-release.

## Targets

| Platform | Target triple | Runner |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | `macos-14` |
| macOS Intel | `x86_64-apple-darwin` | `macos-13` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `ubuntu-22.04` |
| Linux arm64 | `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` |

All four build natively (no cross-compilation), so the tree-sitter C grammars compile with the
host toolchain. The Linux arm64 runner (`ubuntu-22.04-arm`) is free for public repositories; a
private repo needs Arm runners enabled, otherwise drop that matrix row.

## Dry run

Trigger the workflow manually — **Actions → Release → Run workflow** — to build and upload the
artifacts *without* publishing. The `release` job is guarded by `if: github.ref_type == 'tag'`,
so only a real tag push creates a GitHub Release.
