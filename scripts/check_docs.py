#!/usr/bin/env python3
"""Structure check for the GitBook docs tree (stdlib only).

Rules:
  a. Every `](target.md)` link in <root>/SUMMARY.md resolves to an existing
     file and appears exactly once.
  b. SUMMARY.md, README.md and every .md file listed in SUMMARY.md: each
     relative link stays inside <root> and exists. Unlisted files are ignored.
  c. .gitbook.yaml (next to <root>) declares root, readme and summary.
"""
import argparse
import os
import re
import sys

LINK = re.compile(r"\]\(([^)\s]+)")
SCHEME = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")


def _targets(path):
    with open(path, encoding="utf-8") as f:
        text = f.read()
    for raw in LINK.findall(text):
        if raw.startswith("#") or SCHEME.match(raw):
            continue
        target = raw.split("#", 1)[0]
        if target:
            yield target


def _gitbook_keys(path):
    keys = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if ":" in line:
                k, v = line.split(":", 1)
                keys[k.strip()] = v.strip().strip("'\"")
    return keys


def check(root):
    """Return a list of problem messages for the docs tree at `root`."""
    root = os.path.normpath(root)
    problems = []
    summary = os.path.join(root, "SUMMARY.md")
    listed = []

    if os.path.isfile(summary):
        seen = {}
        for t in _targets(summary):
            norm = os.path.normpath(t)
            seen[norm] = seen.get(norm, 0) + 1
        for norm, n in seen.items():
            if n > 1:
                problems.append(f"SUMMARY.md: {norm} listed {n} times")
            if not os.path.isfile(os.path.join(root, norm)):
                problems.append(f"SUMMARY.md: {norm} does not exist")
            elif norm.endswith(".md"):
                listed.append(norm)
    else:
        problems.append(f"{summary} is missing")

    readme = os.path.join(root, "README.md")
    if not os.path.isfile(readme):
        problems.append(f"{readme} is missing")

    root_abs = os.path.abspath(root)
    for rel in ["SUMMARY.md", "README.md"] + listed:
        path = os.path.join(root, rel)
        if not os.path.isfile(path):
            continue
        base = os.path.dirname(os.path.abspath(path))
        for t in _targets(path):
            dest = os.path.normpath(os.path.join(base, t))
            if os.path.commonpath([root_abs, dest]) != root_abs:
                problems.append(f"{rel}: link {t} escapes {root}")
            elif not os.path.exists(dest):
                problems.append(f"{rel}: link {t} does not exist")

    cfg = os.path.join(os.path.dirname(root_abs), ".gitbook.yaml")
    if not os.path.isfile(cfg):
        problems.append(".gitbook.yaml is missing")
    else:
        keys = _gitbook_keys(cfg)
        want = {
            "root": f"./{os.path.basename(root_abs)}/",
            "readme": "README.md",
            "summary": "SUMMARY.md",
        }
        for k, v in want.items():
            if keys.get(k) != v:
                problems.append(f".gitbook.yaml: {k} must be {v}, found {keys.get(k)!r}")
    return problems


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", default="docs")
    args = ap.parse_args(argv)
    problems = check(args.root)
    if problems:
        for p in problems:
            print(p)
        return 1
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
