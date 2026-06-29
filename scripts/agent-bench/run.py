#!/usr/bin/env python3
"""Agent A/B benchmark — same agent, same tasks, same repo, WITH vs WITHOUT the
codeindex MCP server. Trustworthy by construction (see README.md): the only
difference between the two arms is whether the codeindex tools are available; the
model, prompt, and repo start-state are identical, every run starts from a clean
`git reset --hard`, success is an objective check command, tokens come straight from
Claude Code's own JSON, and every task is reported (no cherry-picking).

  python3 run.py --repo /path/to/ts/repo [--task T1-rename] [--runs 1]

Requires: the `claude` CLI on PATH, a release build of codeindex-rs, and the target
repo being a clean git working tree.
"""
import argparse, json, os, pathlib, statistics, subprocess, sys, time

HERE = pathlib.Path(__file__).parent
TASKS = json.loads((HERE / "tasks.json").read_text())
MCP_CONFIG = str(HERE / "codeindex.mcp.json")
ROOT = HERE.parent.parent
RUST = str(ROOT / "target/release/codeindex-rs")

# The claude CLI. Override via $CLAUDE_BIN if `claude` on PATH is a broken stub.
CLAUDE = os.environ.get("CLAUDE_BIN", "claude")
BASE_TOOLS = "Read,Grep,Glob,Edit,Write,Bash"
CI_TOOLS = ",".join([
    "mcp__codeindex__retrieve_context",
    "mcp__codeindex__describe_architecture",
    "mcp__codeindex__list_anchors",
    "mcp__codeindex__apply_edits",
])


def sh(cmd, cwd=None, env=None):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)


def reset(repo, base):
    sh(["git", "reset", "--hard", base], cwd=repo)
    # keep the prebuilt .codeindex-rs index across resets
    sh(["git", "clean", "-fdq", "-e", ".codeindex-rs"], cwd=repo)


def run_agent(repo, prompt, with_codeindex):
    cmd = [CLAUDE, "-p", prompt, "--output-format", "json",
           "--max-turns", "40", "--dangerously-skip-permissions"]
    tools = BASE_TOOLS + ("," + CI_TOOLS if with_codeindex else "")
    if with_codeindex:
        cmd += ["--mcp-config", MCP_CONFIG]
    cmd += ["--allowedTools", tools]

    t = time.time()
    r = sh(cmd, cwd=repo)
    dur = time.time() - t
    try:
        out = json.loads(r.stdout)
        u = out.get("usage", {})
        intok = (u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0)
                 + u.get("cache_creation_input_tokens", 0))
        return {"in": intok, "out": u.get("output_tokens", 0),
                "turns": out.get("num_turns", 0), "cost": out.get("total_cost_usd", 0.0), "dur": dur}
    except Exception:
        return {"in": 0, "out": 0, "turns": 0, "cost": 0.0, "dur": dur, "err": r.stderr[:200]}


def check(repo, cmd):
    return sh(["bash", "-c", cmd], cwd=repo).returncode == 0


def preflight():
    """Fail loudly if the `claude` CLI can't actually run — never report silent 0s."""
    r = sh([CLAUDE, "-p", "reply with exactly: ok", "--output-format", "json"])
    try:
        json.loads(r.stdout)
        return True
    except Exception:
        print("ERROR: `claude` did not return valid JSON — is Claude Code installed and working?")
        print("  stdout:", (r.stdout or "")[:200])
        print("  stderr:", (r.stderr or "")[:300])
        return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--task")
    ap.add_argument("--runs", type=int, default=1)
    args = ap.parse_args()
    repo = os.path.abspath(args.repo)
    if not preflight():
        sys.exit(1)
    base = sh(["git", "rev-parse", "HEAD"], cwd=repo).stdout.strip()

    print(f"# Agent A/B — codeindex MCP on/off\nrepo: {repo} @ {base[:8]}\n")
    print("Building the codeindex index for the WITH arm …")
    sh([RUST, "index", repo],
       env={**os.environ, "CI_NPM_CACHE": "/tmp/ci-npm-cache"})

    rows = []
    for task in TASKS:
        if args.task and task["id"] != args.task:
            continue
        agg = {"without": [], "with": []}
        for _ in range(args.runs):
            for cond, withci in (("without", False), ("with", True)):
                reset(repo, base)
                m = run_agent(repo, task["prompt"], withci)
                m["ok"] = check(repo, task["check"])
                agg[cond].append(m)
        reset(repo, base)
        rows.append((task["id"], agg))

    # ── report ──────────────────────────────────────────────────────────────
    def med(xs, k):
        return statistics.median(x[k] for x in xs) if xs else 0

    print("\n| task | arm | in_tok | out_tok | turns | ok |")
    print("|---|---|--:|--:|--:|:--:|")
    tot = {"without": {"in": 0, "out": 0}, "with": {"in": 0, "out": 0}}
    passes = {"without": 0, "with": 0}
    n = 0
    for tid, agg in rows:
        n += 1
        for cond in ("without", "with"):
            xs = agg[cond]
            ok = sum(1 for x in xs if x["ok"])
            print(f"| {tid} | {cond} | {med(xs,'in'):.0f} | {med(xs,'out'):.0f} | {med(xs,'turns'):.0f} | {ok}/{len(xs)} |")
            tot[cond]["in"] += med(xs, "in")
            tot[cond]["out"] += med(xs, "out")
            passes[cond] += ok

    def pct(a, b):
        return f"{(b - a) / a * 100:+.0f}%" if a else "n/a"

    print("\n## Totals (median per task, summed)\n")
    print(f"- input tokens:  without **{tot['without']['in']:.0f}** · with **{tot['with']['in']:.0f}** ({pct(tot['without']['in'], tot['with']['in'])})")
    print(f"- output tokens: without **{tot['without']['out']:.0f}** · with **{tot['with']['out']:.0f}** ({pct(tot['without']['out'], tot['with']['out'])})")
    print(f"- success: without {passes['without']} · with {passes['with']} (of {n*args.runs} each)")


if __name__ == "__main__":
    main()
