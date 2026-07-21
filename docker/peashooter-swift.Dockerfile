# Peashooter swift gate image — the Swift toolchain (`swift build` gate) + sourcekit-lsp (rename),
# both of which ship in the official Swift image, so a host needs neither. Built locally:
#
#   docker build -f docker/peashooter-swift.Dockerfile -t peashooter-swift docker/
#
# This is the heaviest image (~2.5GB): the Swift toolchain is large and can't be slimmed without
# losing the compiler the gate needs. Pulled lazily, only for a repo that actually has Swift.
# docs/container-gate-spec.md §9b.
FROM swift:6.0
# sourcekit-lsp is already on PATH in the base image (/usr/bin/sourcekit-lsp) — nothing to add.
