#!/usr/bin/env python3
"""Multi-language retrieval benchmark (Batch 6).

The question: in a mixed-language repo, does indexing EVERY language make its files
retrievable? This is the retrieval-visible effect of the extension→provider registry —
before it, one provider per repo meant only one language got indexed and the rest were
invisible to search.

A/B on the same fixture + same tasks, one variable — which provider(s) index:
  * single  — `CI_LANG=rust` forces one provider (the old one-language-per-repo behavior)
  * multi   — auto-detect, so every present language indexes (the registry)

  cargo build --release && python3 scripts/multilang-bench/run.py

Needs the Model2Vec model ($CI_MODEL_DIR, else the sibling repo default). Rust + Python
index in-process (no external tooling); TypeScript is in the fixture too but only indexes
when Node + scip-typescript are available — it's reported as absent otherwise.
"""
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
FIX = os.path.join(HERE, "fixture")
RUST = os.path.join(ROOT, "target/release/marksman")
K = 5

# (task, the file that should surface, its language)
CASES = [
    ("tokenize raw source text into a stream of tokens", "src/lexer.rs", "rust"),
    ("build an abstract syntax tree from parsed tokens", "src/parser.rs", "rust"),
    ("run background jobs periodically on a fixed interval", "jobs/scheduler.py", "python"),
    ("a first in first out in-memory task queue", "jobs/queue.py", "python"),
    ("http server routing incoming requests to handlers", "api/server.ts", "ts"),
    ("validate a bearer token and decode a jwt", "api/auth.ts", "ts"),
]
LANGS = ["rust", "python", "ts"]


def sh(cmd, env):
    return subprocess.run(cmd, env=env, capture_output=True, text=True)


def reindex(env_extra):
    shutil.rmtree(os.path.join(FIX, ".codeindex-rs"), ignore_errors=True)
    sh([RUST, "index", FIX], {**os.environ, **env_extra})


def top_files(task, env_extra):
    r = sh([RUST, "retrieve", FIX, task, "--json", "--top", str(K)], {**os.environ, **env_extra})
    try:
        return [e["file"] for e in json.loads(r.stdout).get("entries", [])][:K]
    except json.JSONDecodeError:
        return []


def run_arm(env_extra):
    reindex(env_extra)
    return [(lang, expect in top_files(task, env_extra)) for task, expect, lang in CASES]


def main():
    if not os.path.exists(RUST):
        sys.exit(f"build first: `cargo build --release` ({RUST} missing)")

    arms = [("single (CI_LANG=rust)", {"CI_LANG": "rust"}), ("multi (auto)", {})]
    data = {name: run_arm(env) for name, env in arms}

    print("# Multi-language retrieval (Batch 6)\n")
    print(f"fixture: {os.path.relpath(FIX, ROOT)} · {len(CASES)} tasks · hit@{K}\n")
    print("| language | single (rust only) | multi (all langs) |")
    print("|---|--:|--:|")
    for lang in LANGS:
        cols = []
        for name, _ in arms:
            hits = sum(1 for l, h in data[name] if l == lang and h)
            tot = sum(1 for l, _ in data[name] if l == lang)
            cols.append(f"{hits}/{tot}")
        print(f"| {lang} | {cols[0]} | {cols[1]} |")

    print()
    for name, _ in arms:
        hits = sum(1 for _, h in data[name] if h)
        print(f"- **{name}**: {hits}/{len(CASES)} tasks retrieved (hit@{K})")
    if all(not h for l, h in data["multi (auto)"] if l == "ts"):
        print("\n_TypeScript files did not index (Node / scip-typescript unavailable); "
              "the rust↔python A/B still shows the registry effect._")


if __name__ == "__main__":
    main()
