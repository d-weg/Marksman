# Peashooter ts image — the full TypeScript toolchain at PINNED versions, so a host running
# Peashooter needs no Node at all (M6: unlike the other languages, TS's READ path — the
# scip-typescript producer — needs the toolchain too, so this image serves indexing AND the
# gate). Built locally:
#
#   docker build -f docker/peashooter-ts.Dockerfile -t peashooter-ts docker/
#
# Global installs expose bare `scip-typescript` and `tsgo` on PATH so the containerized
# provider resolves them by name (docs/container-gate-spec.md §9b). The gate tier in-container
# is tsgo — the fastest tier, and the only one that transplants (ts-morph stages an npm
# install at runtime; tsls can't resolve a global typescript install).
#
# VERSIONS MUST MATCH crates/langs/lang-ts/src/engine.rs (SCIP_TS_VERSION, TSGO_VERSION,
# TYPESCRIPT_VERSION) — the pin is the point (contract §10): the verdict and the artifact are
# tied to a known toolchain, immune to npm "latest" drift.
FROM node:22-slim

RUN npm install -g --no-fund --no-audit \
      @sourcegraph/scip-typescript@0.4.0 \
      typescript@6.0.3 \
      @typescript/native-preview@7.0.0-dev.20260707.2 \
 && scip-typescript --version \
 && tsgo --version
