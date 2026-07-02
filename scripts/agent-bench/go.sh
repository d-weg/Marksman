#!/usr/bin/env bash
# One-shot runner (marksman agent-bench): paste your API key into scripts/agent-bench/.key (gitignored),
# then run this. It wires up CLAUDE_BIN + the model and calls the harness against
# the prepared /tmp/bench-target clone.
#
#   echo "sk-ant-..." > scripts/agent-bench/.key
#   bash scripts/agent-bench/go.sh --task T1-rename --runs 1     # cheap validation
#   bash scripts/agent-bench/go.sh --runs 3                      # full run
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

# API key: from env, else from the .key file.
if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -f "$HERE/.key" ]; then
  ANTHROPIC_API_KEY="$(tr -d ' \r\n' < "$HERE/.key")"
fi
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "No API key. Paste it:  echo \"sk-ant-...\" > $HERE/.key   (or export ANTHROPIC_API_KEY)" >&2
  exit 1
fi
export ANTHROPIC_API_KEY

# The real claude binary (PATH `claude` is a broken stub here); pick the latest installed.
export CLAUDE_BIN="$(ls -d "$HOME/Library/Application Support/Claude/claude-code"/*/claude.app/Contents/MacOS/claude 2>/dev/null | sort -V | tail -1)"
export CLAUDE_MODEL="${CLAUDE_MODEL:-claude-sonnet-4-6}"
export CI_NPM_CACHE="/tmp/ci-npm-cache"
# cargo on PATH for the harness AND the agent: T7-multilang's check runs `cargo check`, and the
# baseline arm needs the same toolchain the rust arm's MCP server finds via its own fallback.
# (First T7 run failed BOTH arms on `command not found: cargo` in the check subprocess.)
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH"
# The Node codeindex checkout (the `ts` arm + the default bench target repo) and the embedding
# model dir — override both via env. Model fallback order: the README's install location, then
# a model bundled inside the TS checkout.
export CODEINDEX_TS_DIR="${CODEINDEX_TS_DIR:-$HOME/codeindex}"
if [ -z "${CI_MODEL_DIR:-}" ]; then
  for cand in "$HOME/.marksman/models/potion-code-16M" "$CODEINDEX_TS_DIR/.models/potion-code-16M"; do
    [ -d "$cand" ] && CI_MODEL_DIR="$cand" && break
  done
fi
export CI_MODEL_DIR="${CI_MODEL_DIR:?no embedding model found — set CI_MODEL_DIR (see README: Get the embedding model)}"

# Prepare the disposable clone if missing (e.g. after a reboot clears /tmp).
TARGET=/tmp/bench-target
if [ ! -d "$TARGET/.git" ]; then
  echo "preparing disposable clone at $TARGET …"
  rm -rf "$TARGET"
  git clone -q "$CODEINDEX_TS_DIR" "$TARGET"
  ln -sfn "$CODEINDEX_TS_DIR/node_modules" "$TARGET/node_modules"
fi

cd "$HERE/../.."
# The bench runs the RELEASE binaries (both the MCP server in codeindex-rust.mcp.json and the
# CLI indexer in run.py) — rebuild them so a run never measures a stale build. Incremental, so
# this is seconds when nothing changed. cargo may not be on a non-interactive PATH; fall back
# to the rustup default location rather than fail the bench.
CARGO="$(command -v cargo || true)"
[ -z "$CARGO" ] && [ -x "$HOME/.cargo/bin/cargo" ] && CARGO="$HOME/.cargo/bin/cargo"
if [ -n "$CARGO" ]; then
  "$CARGO" build --release -p ci-mcp -p ci-cli
else
  echo "warning: cargo not found — running with the EXISTING release binaries (may be stale)" >&2
fi
exec python3 scripts/agent-bench/run.py --repo "$TARGET" "$@"
