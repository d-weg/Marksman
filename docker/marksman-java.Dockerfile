# Marksman java gate image — the JDK gate (javac / javax.tools sidecar) + jdtls (cross-file
# rename), so a host running Marksman needs neither installed. Built locally:
#
#   docker build -f docker/marksman-java.Dockerfile -t marksman-java docker/
#
# Then Marksman runs the java toolchain inside it with CI_SANDBOX=oci (docs/container-gate-spec.md
# §9b). Java 21 is jdtls's runtime floor and also runs the JEP-330 single-file javax.tools sidecar.
FROM eclipse-temurin:21-jdk

# python3 is the jdtls launcher's runtime; curl/ca-certificates fetch the server.
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# jdtls (Eclipse JDT language server) — the rename/willRename engine. Snapshot "latest" gets it
# working; pin to a dated milestone tarball for reproducible verdicts (a later hardening).
RUN mkdir -p /opt/jdtls \
 && curl -fsSL https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz \
    | tar -xz -C /opt/jdtls \
 && ln -sf /opt/jdtls/bin/jdtls /usr/local/bin/jdtls

# jdtls writes its own config/cache under $HOME — keep it off the bind-mounted repo.
ENV HOME=/root
