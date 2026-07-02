#!/usr/bin/env python3
"""Analyze saved agent-bench transcripts (stream-json) to understand HOW each arm spent its
turns/tokens — the qualitative side the token table can't show.

For every transcript it prints: token totals, the ordered tool-call sequence (with response
sizes), which `apply_edits` ACTIONS the agent chose (rename / replace_text / set_body / …),
and whether it read/listed/retrieved BEFORE its first edit (the costly "read-then-edit" tell).

  python3 analyze.py <dir>                 # all *.jsonl in a dir
  python3 analyze.py a.jsonl b.jsonl       # specific files
"""
import json, sys, os, glob

READ_TOOLS = {"Read", "Grep", "Glob", "ci:retrieve_context", "ci:list_anchors", "ci:read_node", "ToolSearch"}
EDIT_TOOLS = {"Edit", "Write", "ci:apply_edits"}


def short(nm):
    return nm.replace("mcp__marksman__", "ci:")


def load(path):
    out = []
    for line in open(path):
        line = line.strip()
        if line:
            try:
                out.append(json.loads(line))
            except Exception:
                pass
    return out


def analyze(path):
    ev = load(path)
    res = next((e for e in reversed(ev) if e.get("type") == "result"), {})
    u = res.get("usage", {})
    intok = u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)
    out_tok, turns = u.get("output_tokens", 0), res.get("num_turns", 0)

    names, seq, edits, reasoning = {}, [], [], []
    for e in ev:
        msg = e.get("message", e)
        cont = msg.get("content")
        if not isinstance(cont, list):
            continue
        for b in cont:
            if not isinstance(b, dict):
                continue
            t = b.get("type")
            if t == "text" and b.get("text", "").strip():
                reasoning.append(b["text"].strip())
            elif t == "tool_use":
                nm = short(b.get("name", "?"))
                names[b.get("id")] = nm
                seq.append([nm, len(json.dumps(b.get("input", {}))), None])
                if "apply_edits" in nm:
                    for a in b.get("input", {}).get("actions", []):
                        tgt = a.get("target")
                        edits.append(a.get("action", "?") + (f":{tgt}" if tgt else ""))
            elif t == "tool_result":
                c = b.get("content")
                txt = c if isinstance(c, str) else json.dumps(c)
                nm = names.get(b.get("tool_use_id"))
                for s in reversed(seq):
                    if s[2] is None and s[0] == nm:
                        s[2] = len(txt)
                        break

    # read-then-edit tell: any READ tool fired before the first EDIT tool?
    first_edit = next((i for i, s in enumerate(seq) if s[0] in EDIT_TOOLS), None)
    read_before = first_edit is not None and any(
        s[0] in (READ_TOOLS - {"ToolSearch"}) for s in seq[:first_edit]
    )

    print(f"### {os.path.basename(path)}")
    print(f"  in={intok:>7}  out={out_tok:>5}  turns={turns}")
    print("  seq: " + "  →  ".join(f"{s[0]}[{(s[2] or 0)//4}t]" for s in seq))
    if edits:
        print(f"  edit actions: {', '.join(edits)}")
    print(f"  read-before-edit: {'YES (could be avoidable)' if read_before else 'no'}")
    if reasoning:
        print(f"  first reasoning: {reasoning[0][:140]}")
    print()


def main():
    paths = []
    for a in sys.argv[1:]:
        paths += sorted(glob.glob(os.path.join(a, "*.jsonl"))) if os.path.isdir(a) else [a]
    if not paths:
        print("usage: python3 analyze.py <dir|files...>")
        return
    for p in paths:
        analyze(p)


if __name__ == "__main__":
    main()
