# Changelog

All notable changes to **Peashooter MCP** are recorded here. The format loosely follows
[Keep a Changelog](https://keepachangelog.com/); releases are git tags `vX.Y.Z` and each one
ships prebuilt binaries (see [docs/releasing.md](docs/releasing.md)).

## [Unreleased]

### Added
- **Release pipeline** — pushing a `vX.Y.Z` tag builds `peashooter` + `peashooter-mcp`
  binaries for macOS (arm64/x64) and Linux (x64/arm64) and publishes them, with SHA256
  checksums, to a GitHub Release.

### Changed
- **Renamed the project Marksman → Peashooter MCP** to avoid the collision with the
  well-known Marksman Markdown LSP. Binaries are now `peashooter` (CLI) and `peashooter-mcp`
  (MCP server); state lives in `.peashooter/`, the env override is `PEASHOOTER_ROOT`, and the
  config file is `peashooter.config.json` (the legacy `marksman.config.json` is still read).

<!-- The first tagged release will be v0.1.0; move the notes above under a
     `## [0.1.0] - YYYY-MM-DD` heading when you cut it. -->
