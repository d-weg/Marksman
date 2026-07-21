# Peashooter rust gate image — cargo/rustc (the `cargo check` gate) + rust-analyzer (rename), so a
# host needs neither. Built locally:
#
#   docker build -f docker/peashooter-rust.Dockerfile -t peashooter-rust docker/
#
# rust-analyzer sends `experimental/serverStatus`, which peashooter already waits on — no readiness
# gotcha (unlike jdtls). docs/container-gate-spec.md §9b.
FROM rust:1-slim

# rust-analyzer as a rustup component, exposed on PATH by its bare name for the containerized engine.
RUN rustup component add rust-analyzer \
 && ln -sf "$(rustup which rust-analyzer)" /usr/local/bin/rust-analyzer
