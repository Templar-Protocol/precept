#!/usr/bin/env python3
"""CI driver for curator pool configs (registry/configs/*.json), issue #49.

pool-plan.yml (pull_request_target, base code only) stages each changed config
from the PR head as *data*, runs the deployer in robot mode without approval
(it reads the chain, sends nothing, exits 3) and posts the plan and its
digest as a PR comment. pool-deploy.yml (push to main, protected environment)
finds that comment's digest and runs the same command with --approve-plan;
the deployer re-checks the digest before every send.

Both sides stage the config at the same path and use the same run directory,
because the frozen plan binds both: a different path is a different plan.

Standard library plus the `gh` CLI the GitHub runners ship. No PR code runs.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path("/tmp/precept-pool-deploy")
CONFIGS = ROOT / "configs"
RUNS = ROOT / "runs"
BOT = "github-actions[bot]"
NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")
# The approvable digest lives only in the first line of the bot's own plan
# comment, never searched for in the body: the body quotes config-controlled
# text (the gate's output), which could otherwise plant a digest.
APPROVAL_RE = re.compile(r"<!-- precept-pool-plan:(\S+) sha256=([0-9a-f]{64}) -->")
PASSPHRASES = {
    "testnet": "Test SDF Network ; September 2015",
    "mainnet": "Public Global Stellar Network ; September 2015",
}
PLAN_EXIT = 3
IDENTITY = "pool-deployer"


def plan_marker(run: str) -> str:
    """Prefix every plan comment for `run` starts with, approvable or not."""
    return f"<!-- precept-pool-plan:{run} "


def plan_header(run: str, digest: str | None) -> str:
    return plan_marker(run) + (f"sha256={digest} -->" if digest else "-->")


def gh(*args: str, stdin: str | None = None) -> str:
    return subprocess.run(
        ["gh", *args], input=stdin, capture_output=True, text=True, check=True
    ).stdout


def gh_json(*args: str):
    return json.loads(gh("api", *args))


def run_id(name: str, network: str, head_sha: str) -> str:
    """The run directory name, and the plan's identity: config name, network,
    and the PR head commit the plan was computed for."""
    if not re.fullmatch(r"[0-9a-f]{40}", head_sha):
        raise SystemExit(f"not a commit sha: {head_sha!r}")
    return f"{name}-{network}-{head_sha[:12]}"


def stage(name: str, text: str) -> dict:
    """Write one config as data to the fixed staging path and return its
    entry. The name becomes a path component, so it is checked first."""
    if not NAME_RE.match(name):
        raise SystemExit(f"config name {name!r} must match {NAME_RE.pattern}")
    network = json.loads(text).get("network")
    if network not in PASSPHRASES:
        raise SystemExit(f"{name}: network must be testnet or mainnet, not {network!r}")
    CONFIGS.mkdir(parents=True, exist_ok=True)
    path = CONFIGS / f"{name}.json"
    path.write_text(text)
    return {"name": name, "network": network, "path": str(path)}


def config_name(path: str) -> str | None:
    m = re.fullmatch(r"registry/configs/([^/]+)\.json", path)
    return m.group(1) if m else None


def cmd_stage_pr(a) -> None:
    pr = gh_json(f"repos/{a.repo}/pulls/{a.pr}")
    head_repo, head_sha = pr["head"]["repo"]["full_name"], pr["head"]["sha"]
    files = json.loads(gh("api", "--paginate", "--slurp", f"repos/{a.repo}/pulls/{a.pr}/files"))
    staged = []
    for f in (f for page in files for f in page):
        name = config_name(f["filename"])
        if name is None or f["status"] == "removed":
            continue
        blob = gh_json(f"repos/{head_repo}/contents/{f['filename']}?ref={head_sha}")
        entry = stage(name, base64.b64decode(blob["content"]).decode())
        entry["run"] = run_id(name, entry["network"], head_sha)
        staged.append(entry)
    print(json.dumps(staged))


def cmd_stage_push(a) -> None:
    """Configs added or modified by a push, read from the checkout, each with
    the PR it was merged from (the plan comment lives there)."""
    diff = subprocess.run(
        ["git", "diff", "--name-status", "--no-renames", a.before, a.after, "--", "registry/configs"],
        capture_output=True, text=True, check=True,
    ).stdout
    pulls = gh_json(f"repos/{a.repo}/commits/{a.after}/pulls")
    merged = [p for p in pulls if p.get("merged_at") and p["merge_commit_sha"] == a.after]
    staged = []
    for line in diff.splitlines():
        status, path = line.split("\t", 1)
        name = config_name(path)
        if name is None or status == "D":
            continue
        if len(merged) != 1:
            raise SystemExit(f"{path}: changed by a push that is not exactly one merged PR; deploy it with workflow_dispatch")
        pr = merged[0]
        entry = stage(name, Path(path).read_text())
        entry.update(pr=pr["number"], run=run_id(name, entry["network"], pr["head"]["sha"]))
        staged.append(entry)
    print(json.dumps(staged))


def stellar_dir(a, network: str) -> Path:
    """A throwaway Stellar config dir naming the deployer identity and the
    network entry the deployer reads its RPC from. The plan side holds only
    the public key; the deploy side imports the seed in the workflow."""
    d = Path(a.config_dir)
    if a.account:  # the plan side owns the whole dir: start it fresh
        shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True, exist_ok=True)
    if a.account:
        subprocess.run(
            [a.stellar, "keys", "add", IDENTITY, "--public-key", a.account, "--config-dir", str(d)],
            check=True, capture_output=True,
        )
    subprocess.run(
        [a.stellar, "network", "add", network, "--rpc-url", a.rpc,
         "--network-passphrase", PASSPHRASES[network], "--config-dir", str(d)],
        check=True, capture_output=True,
    )
    return d


def deployer(a, entry: dict, extra: list[str]) -> tuple[int, list[dict]]:
    out = RUNS / entry["run"]
    argv = [
        a.bin, "--out", str(out), "--stellar-bin", a.stellar,
        "market", "deploy", "--config", entry["path"], "--network", entry["network"],
        "--admin-identity", IDENTITY, "--admin-config-dir", a.config_dir, "--robot", *extra,
    ]
    p = subprocess.run(argv, capture_output=True, text=True)
    sys.stderr.write(p.stderr)
    events = [json.loads(l) for l in p.stdout.splitlines() if l.startswith("{")]
    Path(a.events).write_text("".join(json.dumps(e) + "\n" for e in events))
    return p.returncode, events


def cmd_plan(a) -> None:
    entry = json.loads(a.entry)
    stellar_dir(a, entry["network"])
    check = subprocess.run(
        [sys.executable, "registry/registry.py", "check-config", entry["path"],
         "--network", entry["network"], "--rpc", a.rpc],
        capture_output=True, text=True,
    )
    shutil.rmtree(RUNS / entry["run"], ignore_errors=True)
    code, events = deployer(a, entry, ["--no-registry"])
    plan = next((e for e in events if e.get("event") == "plan"), None)
    body = plan_comment(entry, a.head_sha, check.returncode, check.stdout + check.stderr, code, events, plan)
    upsert_comment(a.repo, a.pr, plan_marker(entry["run"]), body)
    if check.returncode != 0 or code != PLAN_EXIT or plan is None:
        raise SystemExit(f"{entry['name']}: config gate or plan failed (check {check.returncode}, plan exit {code})")


def plan_comment(entry, head_sha, check_rc, check_out, code, events, plan) -> str:
    # Approvable only when the gate passed and the deployer stopped exactly at
    # its plan: anything else comments the reason and carries no digest.
    ok = check_rc == 0 and code == PLAN_EXIT and plan is not None
    lines = [plan_header(entry["run"], plan["plan_sha256"] if ok else None),
             f"### Pool deploy plan: `{entry['name']}` ({entry['network']})", ""]
    gate = check_out.strip().replace("`", "'")  # config text cannot close the fence
    lines += [f"Config gate: {'passed' if check_rc == 0 else '**failed**'}", "```", gate, "```"]
    if plan is None:
        last = events[-1] if events else {}
        lines += [f"**No plan** (deployer exit {code}): `{last.get('reason', last)}`"]
        return "\n".join(lines)
    lines += [
        "",
        f"plan sha256: `{plan['plan_sha256']}`" + ("" if ok else " (**not approvable**: the gate failed)"),
        f"head: `{head_sha}`",
        f"run: `{entry['run']}`",
        "",
        "On merge, the deploy job runs this exact plan (`--approve-plan`) and refuses any other. "
        "It never activates the pool: `set_status` stays the curator's call.",
        "",
        "| phase | slot | action | target |",
        "|---|---|---|---|",
    ]
    lines += [f"| {s['phase']} | {s['slot']} | {s['action']} | {s.get('target_role') or ''} |" for s in plan["steps"]]
    if any(s["action"] == "refresh asset" for s in plan["steps"]):
        lines += [
            "",
            "**Price precondition.** Each `refresh asset` step needs an Accepted price: the run stops there "
            "until this pool's feeds are live (keeper or RedStone push to the sources), then resumes with "
            "the `pool-deploy` workflow's *resume* dispatch.",
        ]
    return "\n".join(lines)


def comments(repo: str, pr: int) -> list[dict]:
    pages = json.loads(gh("api", "--paginate", "--slurp", f"repos/{repo}/issues/{pr}/comments"))
    return [c for page in pages for c in page]


def upsert_comment(repo: str, pr: int, marker: str, body: str) -> None:
    mine = [c for c in comments(repo, pr) if c["user"]["login"] == BOT and c["body"].startswith(marker)]
    payload = json.dumps({"body": body})
    if mine:
        gh("api", "-X", "PATCH", f"repos/{repo}/issues/comments/{mine[-1]['id']}", "--input", "-", stdin=payload)
    else:
        gh("api", "-X", "POST", f"repos/{repo}/issues/{pr}/comments", "--input", "-", stdin=payload)


def approved_digest(repo: str, pr: int, run: str) -> str:
    """The digest the merged PR was reviewed with: the header line of the
    bot's latest plan comment for exactly this run (config, network, final
    head). Nothing else counts."""
    mine = [c for c in comments(repo, pr) if c["user"]["login"] == BOT and c["body"].startswith(plan_marker(run))]
    if not mine:
        raise SystemExit(f"PR #{pr} has no plan comment for {run}; nothing was approved")
    m = APPROVAL_RE.fullmatch(mine[-1]["body"].split("\n", 1)[0])
    if not m or m.group(1) != run:
        raise SystemExit(f"PR #{pr}'s plan comment for {run} carries no approvable plan (the gate or plan failed)")
    return m.group(2)


def cmd_approved(a) -> None:
    print(approved_digest(a.repo, a.pr, a.run))


def cmd_deploy(a) -> None:
    entry = json.loads(a.entry)
    stellar_dir(a, entry["network"])
    extra = ["--approve-plan", a.approve]
    if (RUNS / entry["run"] / "deployment-plan.toml").exists():
        extra.append("--resume")
    code, events = deployer(a, entry, extra)
    body = deploy_comment(entry, code, events)
    Path(a.summary).write_text(body)
    if entry.get("pr"):
        upsert_comment(a.repo, entry["pr"], f"<!-- precept-pool-deploy:{entry['run']} -->", body)
    raise SystemExit(code)


def deploy_comment(entry, code, events) -> str:
    run = entry["run"]
    lines = [f"<!-- precept-pool-deploy:{run} -->", f"### Pool deploy: `{entry['name']}` ({entry['network']})", ""]
    done = next((e for e in events if e.get("event") == "done"), None)
    if done:
        lines += ["Deployed.", "", "| role | contract |", "|---|---|"]
        lines += [f"| {k} | `{done[k]}` |" for k in ("pool", "backstop", "factory", "comet", "emitter") if k in done]
        handoff = done.get("handoff")
        if handoff:
            lines += ["", "**The curator signs these from their own wallet to take over:**", ""]
            for acc in handoff.get("accept", []):
                by = f" (before ledger {acc['before_ledger']})" if acc.get("before_ledger") else ""
                lines.append(f"- `{acc['by']}` calls `{acc['method']}()` on `{acc['contract']}`{by}")
            lines += ["", "Activation (`set_status`) is the curator's own call after they accept."]
        return "\n".join(lines)
    last = events[-1] if events else {}
    reason = last.get("reason") or last.get("note") or "no event"
    lines += [
        f"**Stopped** (exit {code}): `{reason}`",
        *(f"- {v}" for v in last.get("violations", [])),
        "",
        f"The journal is kept. Fix the cause, then run the `pool-deploy` workflow with "
        f"`run={run}` to resume; settled steps are not sent again.",
    ]
    return "\n".join(lines)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("stage-pr"); s.add_argument("--repo", required=True); s.add_argument("--pr", type=int, required=True); s.set_defaults(f=cmd_stage_pr)
    s = sub.add_parser("stage-push"); s.add_argument("--repo", required=True); s.add_argument("--before", required=True); s.add_argument("--after", required=True); s.set_defaults(f=cmd_stage_push)
    s = sub.add_parser("approved"); s.add_argument("--repo", required=True); s.add_argument("--pr", type=int, required=True); s.add_argument("--run", required=True); s.set_defaults(f=cmd_approved)
    for name, f in (("plan", cmd_plan), ("deploy", cmd_deploy)):
        s = sub.add_parser(name)
        s.add_argument("--entry", required=True, help="one staged entry (JSON)")
        s.add_argument("--repo", required=True)
        s.add_argument("--bin", required=True)
        s.add_argument("--stellar", required=True)
        s.add_argument("--rpc", required=True)
        s.add_argument("--config-dir", required=True)
        s.add_argument("--events", required=True)
        s.add_argument("--account", help="deployer public key (plan side only)")
        if name == "plan":
            s.add_argument("--pr", type=int, required=True)
            s.add_argument("--head-sha", required=True)
        else:
            s.add_argument("--approve", required=True)
            s.add_argument("--summary", required=True)
        s.set_defaults(f=f)
    a = p.parse_args()
    a.f(a)


if __name__ == "__main__":
    main()
