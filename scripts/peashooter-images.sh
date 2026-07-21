#!/bin/sh
# Container-mode helper (docs/container-guide.md). Build and check the
# per-language Peashooter images so a host runs the gate/rename toolchain from
# a pinned OCI image instead of installing N language toolchains.
#
#   scripts/peashooter-images.sh check              # runtime + which images exist + pin sync
#   scripts/peashooter-images.sh build              # build every image
#   scripts/peashooter-images.sh build ts rust      # build only these
#   scripts/peashooter-images.sh list               # what each image holds
#
# The images are plain OCI and NEVER pulled — always built locally, so the
# toolchain is auditable. See docker/README.md and docs/container-gate-spec.md.
set -eu
cd "$(dirname "$0")/.."

LANGS="ts rust java php swift"

# Human note per image (kept next to the Dockerfiles' own headers).
lang_note() {
  case "$1" in
    ts)    echo "scip-typescript + tsgo + typescript (~515MB) — the only READ-path image" ;;
    rust)  echo "cargo + rust-analyzer" ;;
    java)  echo "JDK 21 + jdtls (~905MB)" ;;
    php)   echo "php + phpstan + phpactor (~450MB)" ;;
    swift) echo "the Swift toolchain (~2.5GB — the compiler is the size)" ;;
  esac
}

# The container runtime, matching ci_core's resolution order: an explicit
# CI_SANDBOX_RUNTIME wins, else the first of container/docker/podman/nerdctl
# on PATH. Prints the runtime, or nothing when none is found.
detect_runtime() {
  if [ -n "${CI_SANDBOX_RUNTIME:-}" ]; then
    echo "$CI_SANDBOX_RUNTIME"
    return
  fi
  for rt in container docker podman nerdctl; do
    if command -v "$rt" >/dev/null 2>&1; then
      echo "$rt"
      return
    fi
  done
}

# The TS image pins MUST match crates/langs/lang-ts/src/engine.rs — the whole
# point of container mode is a pinned verdict (contract §10). This checks the
# three that drift: scip-typescript, typescript, tsgo (native-preview).
# The @version of an exact npm package in the ts Dockerfile — token-exact
# (a `NAME@` token, so `typescript` never matches inside `scip-typescript`).
img_pin() {
  awk -v pkg="$1" '
    { n = split($0, toks, /[ \t]+/)
      for (i = 1; i <= n; i++) {
        t = toks[i]; sub(/\\$/, "", t)
        if (index(t, pkg "@") == 1) { print substr(t, length(pkg) + 2); exit }
      } }' docker/peashooter-ts.Dockerfile
}

check_ts_pins() {
  eng="crates/langs/lang-ts/src/engine.rs"
  ok=0
  for pair in \
    "SCIP_TS_VERSION|@sourcegraph/scip-typescript" \
    "TYPESCRIPT_VERSION|typescript" \
    "TSGO_VERSION|@typescript/native-preview"; do
    const="${pair%|*}"; pkg="${pair#*|}"
    codev=$(grep -oE "${const}: &str = \"[^\"]+\"" "$eng" | sed -E 's/.*"([^"]+)"/\1/')
    imgv=$(img_pin "$pkg")
    if [ "$codev" = "$imgv" ] && [ -n "$codev" ]; then
      echo "  pin OK   $const = $codev"
    else
      echo "  PIN DRIFT  $const: code=$codev image=$imgv  (bump both together)"
      ok=1
    fi
  done
  return $ok
}

image_exists() {
  rt="$1"; img="$2"
  "$rt" image inspect "$img" >/dev/null 2>&1
}

cmd="${1:-check}"
shift 2>/dev/null || true

case "$cmd" in
  list)
    echo "Peashooter language images (build only what your repos need):"
    for l in $LANGS; do printf "  peashooter-%-6s %s\n" "$l" "$(lang_note "$l")"; done
    ;;

  check)
    rt=$(detect_runtime)
    if [ -z "$rt" ]; then
      echo "runtime:   NONE on PATH (container/docker/podman/nerdctl) — CI_SANDBOX=oci would"
      echo "           warn and stay on the host. Install one, or set CI_SANDBOX_RUNTIME."
    else
      echo "runtime:   $rt"
      echo "images:"
      for l in $LANGS; do
        if image_exists "$rt" "peashooter-$l"; then
          echo "  present  peashooter-$l"
        else
          echo "  missing  peashooter-$l   (build: scripts/peashooter-images.sh build $l)"
        fi
      done
    fi
    echo "ts pins:"
    check_ts_pins || echo "  -> fix the drift before building peashooter-ts, or the verdict won't be reproducible"
    ;;

  build)
    rt=$(detect_runtime)
    [ -n "$rt" ] || { echo "no container runtime found (container/docker/podman/nerdctl)" >&2; exit 1; }
    targets="${*:-$LANGS}"
    # TS drift is fatal for a build (the image would pin a verdict the code
    # doesn't expect); check before building ts.
    case " $targets " in
      *" ts "*) check_ts_pins || { echo "refusing to build peashooter-ts with drifted pins" >&2; exit 1; } ;;
    esac
    for l in $targets; do
      f="docker/peashooter-$l.Dockerfile"
      [ -f "$f" ] || { echo "unknown language '$l' (have: $LANGS)" >&2; exit 1; }
      echo "== building peashooter-$l  ($(lang_note "$l"))"
      "$rt" build -f "$f" -t "peashooter-$l" docker/
    done
    echo "done. Enable with:  CI_SANDBOX=oci peashooter-mcp --root /path/to/repo"
    ;;

  *)
    echo "usage: $0 {check|build [langs...]|list}" >&2
    exit 2
    ;;
esac
