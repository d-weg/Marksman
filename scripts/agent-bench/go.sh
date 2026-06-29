#!/usr/bin/env bash
# One-shot runner: paste your API key into scripts/agent-bench/.key (gitignored),
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
export CODEINDEX_TS_DIR="${CODEINDEX_TS_DIR:-/Users/davi.vasconcelos/codeindex}"
export CI_MODEL_DIR="${CI_MODEL_DIR:-/Users/davi.vasconcelos/codeindex/.models/potion-code-16M}"

cd "$HERE/../.."
exec python3 scripts/agent-bench/run.py --repo /tmp/bench-target "$@"
