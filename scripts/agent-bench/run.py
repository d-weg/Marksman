#!/usr/bin/env python3
"""Agent A/B/C benchmark — same agent, same tasks, same repo, across arms:
  baseline  (no codeindex)   ·   rust (codeindex-rs MCP)   ·   ts (codeindex Node MCP)

Trustworthy by construction (see README.md): the only thing that varies between arms is
which MCP server is loaded; model, prompt, and repo start-state are identical; every run
starts from `git reset --hard`; success is an objective `check` command; tokens come straight
from Claude Code's own JSON; every task is reported.

  python3 run.py --repo /path/to/ts/repo [--task T1-rename] [--runs 3] [--arms baseline,rust,ts]

Requires a working `claude` CLI (set $CLAUDE_BIN if PATH `claude` is a broken stub) and, for
headless runs, an $ANTHROPIC_API_KEY (org policy disables subscription headless).
"""
import argparse, json, os, pathlib, shutil, statistics, subprocess, sys, tempfile, time

HERE = pathlib.Path(__file__).parent
TASKS = json.loads((HERE / "tasks.json").read_text())
ROOT = HERE.parent.parent
RUST = str(ROOT / "target/release/codeindex-rs")
CLAUDE = os.environ.get("CLAUDE_BIN", "claude")
TS_DIR = os.environ.get("CODEINDEX_TS_DIR", "/Users/davi.vasconcelos/codeindex")

BASE_TOOLS = "Read,Grep,Glob,Edit,Write,Bash"
CI_TOOLS = ",".join([
    "mcp__codeindex__retrieve_context",
    "mcp__codeindex__describe_architecture",
    "mcp__codeindex__list_anchors",
    "mcp__codeindex__apply_edits",
])

# arm name -> mcp config path (None = no codeindex). Both servers are named "codeindex"
# so the same mcp__codeindex__* tool allow-list works for either.
ARMS = {
    "baseline": None,
    "rust": str(HERE / "codeindex-rust.mcp.json"),
    "ts": str(HERE / "codeindex-ts.mcp.json"),
}


def sh(cmd, cwd=None, env=None):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)


# Index dirs are NOT plain build artifacts: apply_edits reindexes-on-commit, so a run that
# edits the repo mutates its index. If we merely preserved them across resets, run N+1 would
# search an index that reflects run N's edits (e.g. a symbol already renamed) while the SOURCE
# was reset — a stale, source-inconsistent index that silently penalizes the codeindex arms.
# So we snapshot the freshly-built, base-consistent indexes ONCE and RESTORE them on every
# reset (a file copy, not a reindex — no API cost). Every run starts from an identical index
# that matches the reset source.
INDEX_DIRS = [".codeindex", ".codeindex-rs"]
SNAP = None  # set by snapshot_indexes()


def snapshot_indexes(repo):
    global SNAP
    SNAP = tempfile.mkdtemp(prefix="bench-index-snap-")
    for d in INDEX_DIRS:
        src = os.path.join(repo, d)
        if os.path.isdir(src):
            shutil.copytree(src, os.path.join(SNAP, d))


def reset(repo, base):
    sh(["git", "reset", "--hard", base], cwd=repo)
    sh(["git", "clean", "-fdq"] + sum([["-e", d] for d in INDEX_DIRS], []), cwd=repo)
    # Restore each index from the pristine, base-consistent snapshot.
    if SNAP:
        for d in INDEX_DIRS:
            snap = os.path.join(SNAP, d)
            if os.path.isdir(snap):
                dst = os.path.join(repo, d)
                shutil.rmtree(dst, ignore_errors=True)
                shutil.copytree(snap, dst)


# Nudge the codeindex arms to actually USE the tools — otherwise the benchmark
# measures the agent's whim (it often defaults to grep + manual edits) instead of the
# tool. The baseline gets no such nudge; it uses its standard tools.
PREAMBLE = (
    "You have codeindex MCP tools: retrieve_context (find relevant code for a task), "
    "list_anchors (a file's symbols/anchors), apply_edits (structural edits — rename / "
    "replace_node / move_file — type-checked before they land). Prefer them over grepping "
    "and hand-editing.\n\nTask: "
)


def run_agent(repo, prompt, mcp_config, model):
    full = (PREAMBLE + prompt) if mcp_config else prompt
    cmd = [CLAUDE, "-p", full, "--output-format", "json", "--model", model,
           "--max-turns", "40", "--dangerously-skip-permissions"]
    tools = BASE_TOOLS + ("," + CI_TOOLS if mcp_config else "")
    if mcp_config:
        cmd += ["--mcp-config", mcp_config]
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
        return {"in": 0, "out": 0, "turns": 0, "cost": 0.0, "dur": dur, "err": (r.stderr or r.stdout)[:200]}


def check(repo, cmd):
    return sh(["bash", "-c", cmd], cwd=repo).returncode == 0


def preflight():
    r = sh([CLAUDE, "-p", "reply with exactly: ok", "--output-format", "json"])
    try:
        json.loads(r.stdout)
        return True
    except Exception:
        print("ERROR: `claude` did not return valid JSON. Need a working CLI + $ANTHROPIC_API_KEY.")
        print("  stdout:", (r.stdout or "")[:200])
        print("  stderr:", (r.stderr or "")[:300])
        return False


def build_indexes(repo, arms):
    if "rust" in arms and os.path.exists(RUST):
        print("  building Rust index (.codeindex-rs) …")
        sh([RUST, "index", repo], env={**os.environ, "CI_NPM_CACHE": "/tmp/ci-npm-cache"})
    if "ts" in arms and os.path.isdir(TS_DIR):
        print("  building TS index (.codeindex) …")
        sh(["npm", "run", "index", "--", "--root", repo], cwd=TS_DIR)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--task")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--arms", default="baseline,rust,ts")
    ap.add_argument("--model", default=os.environ.get("CLAUDE_MODEL", "claude-sonnet-4-6"),
                    help="cost lever: claude-haiku-4-5 (cheapest) · claude-sonnet-4-6 (default) · claude-opus-4-8")
    args = ap.parse_args()
    repo = os.path.abspath(args.repo)
    arms = [a for a in args.arms.split(",") if a in ARMS]

    if not preflight():
        sys.exit(1)
    base = sh(["git", "rev-parse", "HEAD"], cwd=repo).stdout.strip()
    print(f"# Agent benchmark — arms: {', '.join(arms)} · model: {args.model}\nrepo: {repo} @ {base[:8]}\n")
    build_indexes(repo, arms)
    snapshot_indexes(repo)  # pristine, base-consistent indexes restored on every reset

    rows = []
    for task in TASKS:
        if args.task and task["id"] != args.task:
            continue
        agg = {a: [] for a in arms}
        for _ in range(args.runs):
            for arm in arms:
                reset(repo, base)
                m = run_agent(repo, task["prompt"], ARMS[arm], args.model)
                m["ok"] = check(repo, task["check"])
                agg[arm].append(m)
        reset(repo, base)
        rows.append((task["id"], agg))

    med = lambda xs, k: statistics.median(x[k] for x in xs) if xs else 0
    # `sec` = median wall-clock for the whole agent run (turns x model+tool latency). It's the
    # time a user actually waits, and rewards a fast tool (e.g. ts-morph ~1s vs a cold LSP);
    # noisier than tokens since it rides on live API latency, so read it alongside turns.
    print("\n| task | arm | in_tok | out_tok | turns | sec | ok |")
    print("|---|---|--:|--:|--:|--:|:--:|")
    tot = {a: {"in": 0, "out": 0, "sec": 0, "pass": 0, "n": 0} for a in arms}
    for tid, agg in rows:
        for arm in arms:
            xs = agg[arm]
            ok = sum(1 for x in xs if x["ok"])
            print(f"| {tid} | {arm} | {med(xs,'in'):.0f} | {med(xs,'out'):.0f} | {med(xs,'turns'):.0f} | {med(xs,'dur'):.0f} | {ok}/{len(xs)} |")
            tot[arm]["in"] += med(xs, "in"); tot[arm]["out"] += med(xs, "out")
            tot[arm]["sec"] += med(xs, "dur")
            tot[arm]["pass"] += ok; tot[arm]["n"] += len(xs)

    pct = lambda a, b: f"{(b-a)/a*100:+.0f}%" if a else "n/a"
    bl = tot.get("baseline")
    print("\n## Totals (median per task, summed)\n")
    print("| arm | input tok | output tok | sec | vs baseline (in/out/sec) | success |")
    print("|---|--:|--:|--:|---|--:|")
    for arm in arms:
        t = tot[arm]
        vs = "—" if (arm == "baseline" or not bl) else \
            f"{pct(bl['in'],t['in'])} / {pct(bl['out'],t['out'])} / {pct(bl['sec'],t['sec'])}"
        print(f"| {arm} | {t['in']:.0f} | {t['out']:.0f} | {t['sec']:.0f} | {vs} | {t['pass']}/{t['n']} |")

    spent = sum(x["cost"] for _, agg in rows for arm in arms for x in agg[arm])
    print(f"\n_actual spend this run: ${spent:.2f} ({args.model})_")


if __name__ == "__main__":
    main()
