#!/usr/bin/env python3
"""codeindex-rs benchmark: tree-sitter overhead, Rust-vs-Node indexing speed,
and retrieval overlap. Run from the repo root after `cargo build --release`.

  python3 scripts/bench.py [OLD_TS_REPO]

OLD_TS_REPO defaults to the sibling Node `codeindex` (the oracle)."""
import os, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OLD = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("CODEINDEX_TS_DIR", os.path.expanduser("~/codeindex"))
RUST = os.path.join(ROOT, "target/release/codeindex-rs")
ENV = {**os.environ, "CI_NPM_CACHE": "/tmp/ci-npm-cache"}
FILE_RE = re.compile(r"(?:src|scripts)/[\w./-]+\.tsx?")

TASKS = [
    "merge bm25 vector and symbol search with reciprocal rank fusion",
    "apply ast anchored structural edits with a type-check gate",
    "compute package aware relevance weighting for a monorepo",
    "build the import graph and expand seeds along it",
]


def timed(cmd, env=ENV, cwd=None):
    t = time.time()
    r = subprocess.run(cmd, env=env, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return time.time() - t, r.returncode


def files_from(out):
    return set(FILE_RE.findall(out))


def rust_retrieve(task):
    r = subprocess.run([RUST, "retrieve", OLD, task], env=ENV, capture_output=True, text=True)
    return files_from(r.stdout)


def node_retrieve(task):
    r = subprocess.run(["npm", "run", "-s", "retrieve", "--", task, "--root", OLD],
                       cwd=OLD, env=ENV, capture_output=True, text=True)
    return files_from(r.stdout)


def main():
    print(f"# codeindex-rs benchmark\n\noracle repo: {OLD}\n")

    # 1. Indexing speed. Warm up once (discard), then take the min of 3 to control
    # for scip-typescript / OS-cache warmup noise.
    timed([RUST, "index", OLD])  # warmup
    best = lambda env_extra: min(timed([RUST, "index", OLD], env={**ENV, **env_extra})[0] for _ in range(3))
    t_nots = best({"CI_NO_TREESITTER": "1"})
    t_rust = best({})  # final index (with tree-sitter) used below
    t_node, rc = timed(["npm", "run", "index", "--", "--root", OLD], cwd=OLD)

    print("## Indexing speed (wall-clock, whole repo; min of 3 after warmup)\n")
    print("| variant | time |")
    print("|---|---|")
    print(f"| Rust · SCIP only (no tree-sitter) | {t_nots:.2f}s |")
    print(f"| Rust · SCIP + tree-sitter | {t_rust:.2f}s |")
    print(f"| tree-sitter overhead | {t_rust - t_nots:+.2f}s ({(t_rust-t_nots)/t_nots*100:+.0f}%) |")
    print(f"| Node (bge, the oracle){' [failed]' if rc else ''} | {t_node:.2f}s |\n")

    # 2. Retrieval overlap (Rust potion vs Node bge).
    print("## Retrieval overlap (Rust vs Node, per task)\n")
    print("| task | rust | node | shared | Jaccard |")
    print("|---|--:|--:|--:|--:|")
    js = []
    for task in TASKS:
        rf, nf = rust_retrieve(task), node_retrieve(task)
        inter, union = rf & nf, rf | nf
        j = len(inter) / len(union) if union else 0.0
        js.append(j)
        print(f"| {task[:42]} | {len(rf)} | {len(nf)} | {len(inter)} | {j:.0%} |")
    print(f"\nmean Jaccard overlap: **{sum(js)/len(js):.0%}**")

    print("\n_Caveat: Rust uses native Model2Vec (potion-code); the Node oracle uses bge-small._")


if __name__ == "__main__":
    main()
