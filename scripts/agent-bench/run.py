#!/usr/bin/env python3
"""Agent A/B/C benchmark — same agent, same tasks, same repo, across arms:
  baseline  (no marksman)   ·   rust (marksman MCP)   ·   ts (the Node prototype MCP)

Trustworthy by construction (see README.md): the only thing that varies between arms is
which MCP server is loaded; model, prompt, and repo start-state are identical; every run
starts from `git reset --hard`; success is an objective `check` command; tokens come straight
from Claude Code's own JSON; every task is reported. Each arm is ONE agent — subagent spawning
is disallowed, so the top-level token/cost numbers account for the whole run.

  python3 run.py --repo /path/to/ts/repo [--task T1-rename] [--runs 3] [--arms baseline,rust,ts]

Requires a working `claude` CLI (set $CLAUDE_BIN if PATH `claude` is a broken stub) and, for
headless runs, an $ANTHROPIC_API_KEY (org policy disables subscription headless).
"""
import argparse, json, os, pathlib, shutil, statistics, subprocess, sys, tempfile, time

HERE = pathlib.Path(__file__).parent
TASKS = json.loads((HERE / "tasks.json").read_text())
ROOT = HERE.parent.parent
RUST = str(ROOT / "target/release/marksman")
CLAUDE = os.environ.get("CLAUDE_BIN", "claude")
TS_DIR = os.environ.get("CODEINDEX_TS_DIR", os.path.expanduser("~/codeindex"))

BASE_TOOLS = "Read,Grep,Glob,Edit,Write,Bash"
CI_TOOLS = ",".join([
    "mcp__marksman__retrieve_context",
    "mcp__marksman__describe_architecture",
    "mcp__marksman__find_symbols",
    "mcp__marksman__list_anchors",
    "mcp__marksman__read_node",
    "mcp__marksman__apply_edits",
])

# The three arms; "baseline" runs with no marksman MCP. Configs are GENERATED at runtime
# (see mcp_config_for) so the repo carries no machine-specific absolute paths. Both servers
# are named "marksman" so the same mcp__marksman__* tool allow-list works for either.
ARMS = ("baseline", "rust", "ts")


def mcp_config_for(arm):
    """Write the arm's MCP config to a temp file and return its path (None for baseline).
    Paths come from this checkout (rust) / $CODEINDEX_TS_DIR (ts) / the caller's env."""
    if arm == "baseline":
        return None
    if arm == "rust":
        env = {"CI_NPM_CACHE": os.environ.get("CI_NPM_CACHE", "/tmp/ci-npm-cache")}
        # Pass ablation/config knobs through to the server (CI_TS_MODE runs the tree-sitter
        # arms of the read-path ablation — see docs/benchmarks.md; the index build inherits
        # the same shell env, so index and server always agree on the mode).
        for k in ("CI_MODEL_DIR", "CI_TS_MODE"):
            if os.environ.get(k):
                env[k] = os.environ[k]
        cfg = {"mcpServers": {"marksman": {"command": str(ROOT / "target/release/marksman-mcp"), "env": env}}}
    else:  # ts — the Node oracle
        cfg = {"mcpServers": {"marksman": {
            "command": os.path.join(TS_DIR, "node_modules/.bin/tsx"),
            "args": [os.path.join(TS_DIR, "src/mcp.ts")],
        }}}
    fd, path = tempfile.mkstemp(prefix=f"bench-{arm}-", suffix=".mcp.json")
    with os.fdopen(fd, "w") as f:
        json.dump(cfg, f)
    return path


def sh(cmd, cwd=None, env=None):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)


# Index dirs are NOT plain build artifacts: apply_edits reindexes-on-commit, so a run that
# edits the repo mutates its index. If we merely preserved them across resets, run N+1 would
# search an index that reflects run N's edits (e.g. a symbol already renamed) while the SOURCE
# was reset — a stale, source-inconsistent index that silently penalizes the marksman arms.
# So we snapshot the freshly-built, base-consistent indexes ONCE and RESTORE them on every
# reset (a file copy, not a reindex — no API cost). Every run starts from an identical index
# that matches the reset source.
INDEX_DIRS = [".codeindex", ".marksman"]


def snapshot_indexes(repo):
    snap = tempfile.mkdtemp(prefix="bench-index-snap-")
    for d in INDEX_DIRS:
        src = os.path.join(repo, d)
        if os.path.isdir(src):
            shutil.copytree(src, os.path.join(snap, d))
    return snap


def reset(repo, base, snap):
    r = sh(["git", "reset", "--hard", base], cwd=repo)
    if r.returncode != 0:
        # A reset that silently fails leaves the previous run's edits in place and every
        # subsequent measurement poisoned — fail the bench, never limp on.
        sys.exit(f"ERROR: git reset failed in {repo}: {r.stderr.strip()}")
    sh(["git", "clean", "-fdq"] + sum([["-e", d] for d in INDEX_DIRS], []), cwd=repo)
    # Restore each index from the pristine, base-consistent snapshot.
    if snap:
        for d in INDEX_DIRS:
            s = os.path.join(snap, d)
            if os.path.isdir(s):
                dst = os.path.join(repo, d)
                shutil.rmtree(dst, ignore_errors=True)
                shutil.copytree(s, dst)


def materialize_fixture(name):
    """Copy a bench fixture (a subdir of this script's dir, named by a task's `fixture` field)
    into a throwaway git repo. NEVER git-reset a fixture in place — it lives inside this
    project's own worktree, so a reset there would clobber the project, not the fixture."""
    src = HERE / name
    if not src.is_dir():
        sys.exit(f"ERROR: task fixture '{name}' not found at {src}")
    tmp = tempfile.mkdtemp(prefix=f"bench-{name}-")
    shutil.copytree(src, tmp, dirs_exist_ok=True)
    sh(["git", "init", "-q"], cwd=tmp)
    sh(["git", "add", "-A"], cwd=tmp)
    sh(["git", "-c", "user.email=bench@local", "-c", "user.name=bench", "commit", "-qm", "base"], cwd=tmp)
    return tmp


# Nudge the marksman arms to actually USE the tools — otherwise the benchmark
# measures the agent's whim (it often defaults to grep + manual edits) instead of the
# tool. The baseline gets no such nudge; it uses its standard tools.
PREAMBLE = (
    "You have marksman MCP tools. They are DEFERRED — load them FIRST, in ONE call, with their FULL "
    "names (a bare name like `select:apply_edits` FAILS — it must be `mcp__marksman__apply_edits`):\n"
    "  ToolSearch  query=\"select:mcp__marksman__apply_edits,mcp__marksman__find_symbols,"
    "mcp__marksman__retrieve_context,mcp__marksman__read_node,mcp__marksman__list_anchors\"\n"
    "What they do: apply_edits (structural + surgical edits — rename / move_file / replace_text / "
    "set_body / replace_node target:body|return|param.N / insert_member — type-checked before landing "
    "when the language has a checker; otherwise applied structurally and the reply carries a "
    "server-side verification scan. TRUST the reply either way — never re-verify by hand), "
    "find_symbols (name -> node-id handles; to disambiguate a name apply_edits called ambiguous), "
    "retrieve_context (find code by concept), read_node (one symbol's full source), list_anchors (a "
    "file's anchors).\n"
    "Then EDIT WITH THE TOOL, not grep+Edit: if the task NAMES the symbol, call apply_edits by name "
    "DIRECTLY — don't locate it first; if the task also gives the FILE, address as `file#name` (e.g. "
    "`src/http/retry.ts#parseResponse`) so it resolves in ONE call; else a bare name works and an ambiguous one "
    "just returns candidate ids to re-issue with. This holds even for "
    "a ONE-LINE change (change a default, fix a value) — use apply_edits replace_text by name; do NOT "
    "reach for Grep/Bash/Read+Edit for a small edit. It's verified server-side and needs no separate search.\n\n"
    "Task: "
)


def run_agent(repo, prompt, mcp_config, model, transcript=None):
    full = (PREAMBLE + prompt) if mcp_config else prompt
    # When capturing a transcript, stream every message (tool_use / tool_result) so we can see
    # exactly which tools the agent called and how big each response was. Otherwise the compact
    # `json` result is all we need for the token table.
    fmt = "stream-json" if transcript else "json"
    # --strict-mcp-config on EVERY arm: the configured servers are the ONLY servers. Without it,
    # user-scope MCP servers on the bench machine leak into the run (a T5 ts-arm run was caught
    # using this repo's own globally-registered tool mid-task) — contaminating both the arm
    # isolation and the token accounting.
    cmd = [CLAUDE, "-p", full, "--output-format", fmt, "--model", model,
           "--max-turns", "40", "--dangerously-skip-permissions", "--strict-mcp-config"]
    if transcript:
        cmd += ["--verbose"]
    tools = BASE_TOOLS + ("," + CI_TOOLS if mcp_config else "")
    if mcp_config:
        cmd += ["--mcp-config", mcp_config]
    # Keep each arm ONE agent: the subagent-spawn tool (`Agent`/`Task`) is available by default even
    # when not in --allowedTools, and the model sometimes reaches for it. That both contaminates
    # "one agent doing everything" AND breaks the token metric — the top-level usage counts only the
    # main agent while `$` counts the subagent too, so a delegated run shows the FEWEST tokens yet
    # the HIGHEST cost. Disallow it so the reported numbers describe the whole run.
    cmd += ["--allowedTools", tools, "--disallowedTools", "Agent,Task"]

    t = time.time()
    r = sh(cmd, cwd=repo)
    dur = time.time() - t
    try:
        if transcript:
            pathlib.Path(transcript).write_text(r.stdout)
            out = next((o for line in reversed(r.stdout.splitlines())
                        if (o := json.loads(line)) and o.get("type") == "result"), {})
        else:
            out = json.loads(r.stdout)
        u = out.get("usage", {})
        intok = (u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0)
                 + u.get("cache_creation_input_tokens", 0))
        return {"in": intok, "out": u.get("output_tokens", 0),
                "turns": out.get("num_turns", 0), "cost": out.get("total_cost_usd", 0.0), "dur": dur}
    except Exception:
        return {"in": 0, "out": 0, "turns": 0, "cost": 0.0, "dur": dur, "err": (r.stderr or r.stdout)[:200]}


def normalize_transcript(path):
    """Write `<path minus .jsonl>.calls.jsonl` — ONE record per API call, deduped.

    The raw stream-json is the CLI's ground truth but it is a TRAP for accounting: an
    assistant message streams as MULTIPLE events (one per content block), each repeating the
    SAME cumulative usage — naive summing triple-counted a bench run's tokens. This sidecar
    is the canonical shape for any analysis. Schema (v1), one JSON object per line:
      {"schema": 1, "call": <0-based index>, "id": <api message id>,
       "usage": {"fresh": n, "cache_write": n, "cache_read": n, "output": n},
       "tools": [{"name": str, "id": str}...], "stop": <stop_reason>}
    plus one TRAILER record {"schema": 1, "call": -1, "id": "result", "usage_total": {...},
    "cost_usd": x} carrying the API's authoritative totals. Caveat: per-call `output` is a
    lower bound (stream chunks snapshot output_tokens mid-message); the trailer's total is
    exact — use it for any output/cost accounting.
    """
    calls = {}   # id -> record (chunks merge: max usage, union of tool blocks)
    order = []
    result_line = None
    for line in pathlib.Path(path).read_text().splitlines():
        try:
            m = json.loads(line)
        except Exception:
            continue
        if m.get("type") == "result":
            result_line = m
            continue
        if m.get("type") != "assistant":
            continue
        msg = m.get("message", {})
        mid = msg.get("id", "?")
        if mid not in calls:
            order.append(mid)
            calls[mid] = {"schema": 1, "call": len(order) - 1, "id": mid,
                          "usage": {"fresh": 0, "cache_write": 0, "cache_read": 0, "output": 0},
                          "tools": [], "stop": None}
        rec = calls[mid]
        u = msg.get("usage", {})
        rec["usage"]["fresh"] = max(rec["usage"]["fresh"], u.get("input_tokens", 0))
        rec["usage"]["cache_write"] = max(rec["usage"]["cache_write"], u.get("cache_creation_input_tokens", 0))
        rec["usage"]["cache_read"] = max(rec["usage"]["cache_read"], u.get("cache_read_input_tokens", 0))
        rec["usage"]["output"] = max(rec["usage"]["output"], u.get("output_tokens", 0))
        rec["stop"] = msg.get("stop_reason") or rec["stop"]
        for c in msg.get("content", []) if isinstance(msg.get("content"), list) else []:
            if isinstance(c, dict) and c.get("type") == "tool_use":
                if not any(t["id"] == c.get("id") for t in rec["tools"]):
                    rec["tools"].append({"name": c.get("name", "?"), "id": c.get("id", "?")})
    out = pathlib.Path(str(path).removesuffix(".jsonl") + ".calls.jsonl")
    body = "".join(json.dumps(calls[mid]) + "\n" for mid in order)
    ru = (result_line or {}).get("usage", {}) if result_line else {}
    trailer = {"schema": 1, "call": -1, "id": "result",
               "usage_total": {"fresh": ru.get("input_tokens", 0),
                                "cache_write": ru.get("cache_creation_input_tokens", 0),
                                "cache_read": ru.get("cache_read_input_tokens", 0),
                                "output": ru.get("output_tokens", 0)},
               "cost_usd": (result_line or {}).get("total_cost_usd")}
    out.write_text(body + json.dumps(trailer) + "\n")
    t = trailer["usage_total"]
    print(f"  normalized: {len(order)} api calls -> {out.name}  "
          f"(in={t['fresh']+t['cache_write']+t['cache_read']} out={t['output']} cost=${trailer['cost_usd']})")


def summarize_transcript(path):
    """Print the agent's tool calls (name + response size) from a saved stream-json transcript —
    so we can see WHERE the tokens went (e.g. a big retrieve_context response re-read each turn)."""
    try:
        lines = pathlib.Path(path).read_text().splitlines()
    except Exception as e:
        print(f"  (no transcript: {e})"); return
    print(f"  tool calls in {os.path.basename(path)}:")
    names = {}  # tool_use_id -> name
    for line in lines:
        try:
            o = json.loads(line)
        except Exception:
            continue
        msg = o.get("message", o)
        for block in (msg.get("content") or []) if isinstance(msg.get("content"), list) else []:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "tool_use":
                names[block.get("id")] = block.get("name", "?")
                inp = json.dumps(block.get("input", {}))
                print(f"    → {block.get('name'):42} in={len(inp):6d}ch")
            elif block.get("type") == "tool_result":
                c = block.get("content")
                txt = c if isinstance(c, str) else json.dumps(c)
                nm = names.get(block.get("tool_use_id"), "result")
                print(f"      ← {nm:40} resp={len(txt):6d}ch (~{len(txt)//4} tok)")


def check(repo, cmd):
    return sh(["bash", "-c", cmd], cwd=repo).returncode == 0


def preflight():
    r = sh([CLAUDE, "-p", "reply with exactly: ok", "--output-format", "json"])
    try:
        out = json.loads(r.stdout)
    except Exception:
        print("ERROR: `claude` did not return valid JSON. Need a working CLI + $ANTHROPIC_API_KEY.")
        print("  stdout:", (r.stdout or "")[:200])
        print("  stderr:", (r.stderr or "")[:300])
        return False
    # An auth/API failure is still valid JSON (`is_error:true`) — catch it here, else every arm
    # silently reports 0 tokens / ok=False and the run looks "done". A common cause: Claude Code
    # authenticating via a stored subscription/OAuth login that the org blocks headless, instead of
    # using $ANTHROPIC_API_KEY — `claude /logout` (or a shell without the OAuth session) forces the
    # API key. See go.sh's header on the org-policy caveat.
    if out.get("is_error") or out.get("api_error_status"):
        print("ERROR: `claude` reached the CLI but the API call FAILED — fix auth before benchmarking.")
        print("  message:", (out.get("result") or "")[:300])
        return False
    return True


def build_indexes(repo, arms):
    if "rust" in arms and os.path.exists(RUST):
        print("  building marksman index (.marksman) …")
        sh([RUST, "index", repo], env={**os.environ, "CI_NPM_CACHE": "/tmp/ci-npm-cache"})
    if "ts" in arms and os.path.isdir(TS_DIR):
        print("  building TS index (.codeindex) …")
        sh(["npm", "run", "index", "--", "--root", repo], cwd=TS_DIR)


def prepare_repo(repo, arms):
    """Reset to a pristine base, build the indexes from scratch, snapshot them. Returns the
    context every run of every task against this repo starts from."""
    # The repo must be a WORKING git checkout — abort loudly if not. The macOS /tmp cleaner
    # reaps files (not dirs) unused ~3 days, which guts an old /tmp clone in place (.git/
    # objects and package.json gone, empty dirs left): rev-parse then returns nothing, the
    # reset silently no-ops, and every run measures agents flailing in wreckage (the
    # 07-04 T2 "regression" — 18 calls of environment archaeology, zero tool signal).
    probe = sh(["git", "rev-parse", "HEAD"], cwd=repo)
    if probe.returncode != 0 or not probe.stdout.strip():
        sys.exit(
            f"ERROR: {repo} is not a usable git repository ({probe.stderr.strip() or 'no HEAD'}).\n"
            f"  A stale /tmp clone was likely gutted by the periodic /tmp cleaner.\n"
            f"  Delete it and re-run — go.sh re-clones a fresh one: rm -rf {repo}"
        )
    base = probe.stdout.strip()
    # Build the snapshot index from a PRISTINE, base-consistent tree. Critical: `marksman
    # index` updates INCREMENTALLY (mtime-keyed), so a stale index left by a prior run — or a
    # dirty working tree — would be snapshotted and then restored on every reset, leaving the
    # index inconsistent with the reset SOURCE (the T6 postmortem). Reset tracked source to
    # base and delete the index dirs so the build is from scratch.
    sh(["git", "reset", "--hard", base], cwd=repo)
    for d in INDEX_DIRS:
        shutil.rmtree(os.path.join(repo, d), ignore_errors=True)
    build_indexes(repo, arms)
    return {"repo": repo, "base": base, "snap": snapshot_indexes(repo)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", help="main benchmark repo; required unless every selected task has its own `fixture`")
    ap.add_argument("--task")
    ap.add_argument("--runs", type=int, default=1)
    # "ts" (the Node prototype Marksman rewrote) is opt-in: it's frozen/unmaintained and exists
    # only as a historical comparison arm — the benchmark that matters is baseline vs rust.
    ap.add_argument("--arms", default="baseline,rust")
    ap.add_argument("--model", default=os.environ.get("CLAUDE_MODEL", "claude-sonnet-4-6"),
                    help="cost lever: claude-haiku-4-5 (cheapest) · claude-sonnet-4-6 (default) · claude-opus-4-8")
    ap.add_argument("--save-transcript", help="dir to dump per-run stream-json transcripts + a tool-usage summary (to see where tokens go)")
    ap.add_argument("--suite", help="language suite(s) for suite-parameterized tasks (comma list, e.g. rust or ts,rust): ONE task identity, the suite just points it at that language's fixture")
    ap.add_argument("--list-tasks", action="store_true", help="print task ids (with available suites) and exit")
    ap.add_argument("--normalize", metavar="DIR", help="write deduped .calls.jsonl sidecars for every raw transcript in DIR and exit (no agent runs)")
    args = ap.parse_args()
    if args.normalize:
        for f in sorted(pathlib.Path(args.normalize).glob("*.run*.jsonl")):
            if f.name.endswith(".calls.jsonl"):
                continue
            print(f.name)
            normalize_transcript(f)
        return
    if args.list_tasks:
        for t in TASKS:
            suites = f"  [suites: {', '.join(sorted(t['suites']))}]" if "suites" in t else ""
            print(f"{t['id']}{suites}")
        return
    arms = [a for a in args.arms.split(",") if a in ARMS]
    tasks = [t for t in TASKS if not args.task or t["id"] == args.task]
    if not tasks:
        sys.exit(f"ERROR: no task matches {args.task!r}")
    # Suite-parameterized tasks: ONE task identity (rename, schema-field, …) bound to a
    # language by --suite — same task, different repo. Each suite binding carries the
    # fixture + the language-native prompt/check; the expanded id is `<task>-<suite>` so
    # reports and transcripts stay distinguishable.
    # --suite without --task selects ONLY the suite tasks — `--suite rust` means "run the
    # rust suite", never "…plus every legacy task alongside it".
    if args.suite and not args.task:
        tasks = [t for t in tasks if "suites" in t]
    expanded = []
    for t in tasks:
        if "suites" not in t:
            if args.suite:
                sys.exit(f"ERROR: task {t['id']!r} is not suite-parameterized — drop --suite (its repo is fixed)")
            expanded.append(t)
            continue
        if not args.suite:
            if args.task:
                sys.exit(f"ERROR: task {t['id']!r} is suite-parameterized — add --suite ({', '.join(sorted(t['suites']))})")
            continue  # no --suite and no explicit --task: skip suite tasks, run the legacy set
        for suite in args.suite.split(","):
            b = t["suites"].get(suite)
            if b is None:
                sys.exit(f"ERROR: task {t['id']!r} has no {suite!r} suite (available: {', '.join(sorted(t['suites']))})")
            expanded.append({
                "id": f"{t['id']}-{suite}",
                "fixture": b["fixture"],
                "prompt": b["prompt"],
                "check": b["check"],
                "why": t.get("why", ""),
                **({"arms": b["arms"]} if "arms" in b else {}),
            })
    tasks = expanded
    if not tasks:
        sys.exit("ERROR: selection left no runnable tasks (suite tasks need --suite)")
    if any("fixture" not in t for t in tasks) and not args.repo:
        sys.exit("ERROR: --repo is required (some selected tasks don't carry their own `fixture`)")

    if not preflight():
        sys.exit(1)
    arm_cfgs = {a: mcp_config_for(a) for a in arms}
    # One prepared context per distinct repo: the main --repo (tasks without `fixture`) plus a
    # throwaway materialized copy per named fixture (e.g. the multilanguage task's mixed repo).
    ctxs = {}
    if any("fixture" not in t for t in tasks):
        ctxs[None] = prepare_repo(os.path.abspath(args.repo), arms)
    for name in sorted({t["fixture"] for t in tasks if "fixture" in t}):
        ctxs[name] = prepare_repo(materialize_fixture(name), arms)
    mode = os.environ.get("CI_TS_MODE", "full")
    print(f"# Agent benchmark — arms: {', '.join(arms)} · model: {args.model} · CI_TS_MODE: {mode}")
    for name, c in ctxs.items():
        print(f"repo[{name or 'main'}]: {c['repo']} @ {c['base'][:8]}")
    print()

    rows = []
    for task in tasks:
        ctx = ctxs[task.get("fixture")]
        # A task may restrict its arms (e.g. the multilang task: the Node oracle is TS-only).
        task_arms = [a for a in arms if a in task.get("arms", arms)]
        agg = {a: [] for a in arms}
        for ri in range(args.runs):
            for arm in task_arms:
                reset(ctx["repo"], ctx["base"], ctx["snap"])
                tx = None
                if args.save_transcript:
                    os.makedirs(args.save_transcript, exist_ok=True)
                    tx = os.path.join(args.save_transcript, f"{task['id']}.{arm}.run{ri}.jsonl")
                m = run_agent(ctx["repo"], task["prompt"], arm_cfgs[arm], args.model, transcript=tx)
                m["ok"] = check(ctx["repo"], task["check"])
                agg[arm].append(m)
                if tx:
                    print(f"[{task['id']} {arm} run{ri}] in={m['in']} out={m['out']} turns={m['turns']} ok={m['ok']}")
                    normalize_transcript(tx)
                    summarize_transcript(tx)
        reset(ctx["repo"], ctx["base"], ctx["snap"])
        rows.append((task["id"], agg))

    med = lambda xs, k: statistics.median(x[k] for x in xs) if xs else 0
    # `sec` = median wall-clock for the whole agent run (turns x model+tool latency). It's the
    # time a user actually waits, and rewards a fast tool (e.g. ts-morph ~1s vs a cold LSP);
    # noisier than tokens since it rides on live API latency, so read it alongside turns.
    # `$` = Claude Code's reported total_cost_usd — the TRUE economic score. It bakes in prompt
    # caching (re-sent context bills at ~10% as cache reads) and output's higher per-token price,
    # so it can diverge from `in_tok`: a many-turn run looks token-heavy yet its re-reads are
    # cheap, while fewer turns mean less (pricey) output. Read `$` as the real headline.
    print("\n| task | arm | in_tok | out_tok | turns | sec | $ | ok |")
    print("|---|---|--:|--:|--:|--:|--:|:--:|")
    tot = {a: {"in": 0, "out": 0, "sec": 0, "cost": 0, "pass": 0, "n": 0} for a in arms}
    for tid, agg in rows:
        for arm in arms:
            xs = agg[arm]
            if not xs:
                continue  # arm not run for this task (task-level `arms` restriction)
            ok = sum(1 for x in xs if x["ok"])
            print(f"| {tid} | {arm} | {med(xs,'in'):.0f} | {med(xs,'out'):.0f} | {med(xs,'turns'):.0f} | {med(xs,'dur'):.0f} | {med(xs,'cost'):.4f} | {ok}/{len(xs)} |")
            tot[arm]["in"] += med(xs, "in"); tot[arm]["out"] += med(xs, "out")
            tot[arm]["sec"] += med(xs, "dur"); tot[arm]["cost"] += med(xs, "cost")
            tot[arm]["pass"] += ok; tot[arm]["n"] += len(xs)

    pct = lambda a, b: f"{(b-a)/a*100:+.0f}%" if a else "n/a"
    bl = tot.get("baseline")
    print("\n## Totals (median per task, summed)\n")
    print("| arm | input tok | output tok | sec | $ cost | vs baseline (in/out/sec/$) | success |")
    print("|---|--:|--:|--:|--:|---|--:|")
    for arm in arms:
        t = tot[arm]
        vs = "—" if (arm == "baseline" or not bl) else \
            f"{pct(bl['in'],t['in'])} / {pct(bl['out'],t['out'])} / {pct(bl['sec'],t['sec'])} / {pct(bl['cost'],t['cost'])}"
        print(f"| {arm} | {t['in']:.0f} | {t['out']:.0f} | {t['sec']:.0f} | {t['cost']:.4f} | {vs} | {t['pass']}/{t['n']} |")

    spent = sum(x["cost"] for _, agg in rows for arm in arms for x in agg[arm])
    print(f"\n_actual spend this run: ${spent:.2f} ({args.model})_")


if __name__ == "__main__":
    main()
