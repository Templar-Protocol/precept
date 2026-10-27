#!/usr/bin/env python3
"""In-git registry of the contracts and WASM hashes Templar deploys.

Files: registry/<network>.jsonl (network = testnet | mainnet), one canonical
JSON object per line. Python 3 standard library only. See registry/README.md.

    registry.py add    --network N FILE|-  [--update]
    registry.py retire --network N --deployment D
    registry.py check
    registry.py verify --network N [--rpc URL]
"""
import argparse
import base64
import binascii
import datetime
import json
import re
import sys
import tempfile
import urllib.request
from pathlib import Path

DIR = Path(__file__).resolve().parent
NETWORKS = {
    "testnet": "https://soroban-testnet.stellar.org",
    # SDF runs no public mainnet RPC; this is a free, keyless public provider
    # listed in the Stellar docs. `--rpc` overrides it.
    "mainnet": "https://mainnet.sorobanrpc.com",
}
STATUSES = ("active", "retired")
# `refs` keys whose target is NOT a Templar-deployed contract and so need not
# be in the registry (e.g. the Pyth Lazer verifier, third-party tokens).
EXTERNAL_REF_KEYS = {"verifier", "token"}

CONTRACT_REQUIRED = {"kind", "network", "id", "role", "deployment", "status", "wasm_hash", "refs", "deployed_at"}
CONTRACT_OPTIONAL = {"label", "admin", "assets"}
WASM_REQUIRED = {"kind", "network", "hash", "artifact", "version", "source"}
WASM_OPTIONAL = {"length"}
SOURCE_KEYS = {"repo", "tag", "asset", "commit", "note"}

HEX64 = re.compile(r"[0-9a-f]{64}\Z")
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
DEPLOYMENT = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}\Z")
ROLE = re.compile(r"[a-z][a-z0-9_]{0,47}\Z")
ARTIFACT = re.compile(r"[a-z][a-z0-9_-]{0,63}\Z")
DATE = re.compile(r"\d{4}-\d{2}-\d{2}\Z")


# ---- strkey -----------------------------------------------------------------

_VERSION = {"C": 2 << 3, "G": 6 << 3}


def _crc16(data):
    crc = 0
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def strkey_encode(prefix, raw32):
    body = bytes([_VERSION[prefix]]) + raw32
    return base64.b32encode(body + _crc16(body).to_bytes(2, "little")).decode()


def strkey_ok(value, prefix):
    if not isinstance(value, str) or len(value) != 56 or value[0] != prefix:
        return False
    try:
        raw = base64.b32decode(value)
    except (binascii.Error, ValueError):
        return False
    return raw[0] == _VERSION[prefix] and _crc16(raw[:33]).to_bytes(2, "little") == raw[33:]


# ---- records ----------------------------------------------------------------


def dump(rec):
    return json.dumps(rec, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def unique_key(rec):
    return (rec["kind"], rec["id"] if rec["kind"] == "contract" else rec["hash"])


def sort_key(rec):
    if rec["kind"] == "contract":
        return ("contract", rec["deployment"], rec["role"], rec["id"])
    return ("wasm", rec["artifact"], rec["hash"], "")


def _str(value):
    return isinstance(value, str) and value != "" and value == value.strip()


def _date_ok(value):
    if not isinstance(value, str) or not DATE.match(value):
        return False
    try:
        datetime.date.fromisoformat(value)
    except ValueError:
        return False
    return True


def _check_source(src):
    if not isinstance(src, dict):
        return ["source: must be an object"]
    errs = []
    extra = set(src) - SOURCE_KEYS
    if extra:
        errs.append(f"source: unknown keys {sorted(extra)}")
    for k in SOURCE_KEYS & set(src):
        if not _str(src[k]):
            errs.append(f"source.{k}: must be a non-empty string")
    if ("tag" in src) != ("asset" in src):
        errs.append("source: tag and asset go together")
    if "tag" in src and "commit" in src:
        errs.append("source: give tag+asset or commit, not both")
    if "commit" in src and not HEX40.match(str(src["commit"])):
        errs.append("source.commit: must be a full 40-hex git commit")
    if not ("tag" in src or "commit" in src or "note" in src):
        errs.append("source: need tag+asset, commit, or a note saying why the source is unknown")
    return errs


def validate_record(rec, network):
    """Schema errors for one record (no cross-record rules)."""
    if not isinstance(rec, dict):
        return ["record must be a JSON object"]
    kind = rec.get("kind")
    if kind == "contract":
        required, optional = CONTRACT_REQUIRED, CONTRACT_OPTIONAL
    elif kind == "wasm":
        required, optional = WASM_REQUIRED, WASM_OPTIONAL
    else:
        return [f'kind: must be "contract" or "wasm", got {kind!r}']
    errs = []
    if missing := required - set(rec):
        errs.append(f"missing keys {sorted(missing)}")
    if extra := set(rec) - required - optional:
        errs.append(f"unknown keys {sorted(extra)}")
    if errs:
        return errs
    if rec["network"] != network:
        errs.append(f"network: {rec['network']!r} in a {network} file")
    if kind == "wasm":
        if not (isinstance(rec["hash"], str) and HEX64.match(rec["hash"])):
            errs.append("hash: must be 64 lowercase hex")
        if not (isinstance(rec["artifact"], str) and ARTIFACT.match(rec["artifact"])):
            errs.append("artifact: must match [a-z][a-z0-9_-]*")
        if not _str(rec["version"]):
            errs.append("version: must be a non-empty string")
        if "length" in rec and not (type(rec["length"]) is int and rec["length"] > 0):
            errs.append("length: must be a positive integer")
        errs += _check_source(rec["source"])
        return errs
    if not strkey_ok(rec["id"], "C"):
        errs.append(f"id: not a valid contract strkey: {rec['id']!r}")
    if not (isinstance(rec["role"], str) and ROLE.match(rec["role"])):
        errs.append("role: must match [a-z][a-z0-9_]*")
    if not (isinstance(rec["deployment"], str) and DEPLOYMENT.match(rec["deployment"])):
        errs.append("deployment: must match [A-Za-z0-9][A-Za-z0-9_.-]*")
    if rec["status"] not in STATUSES:
        errs.append(f"status: must be one of {STATUSES}")
    wh = rec["wasm_hash"]
    if wh is not None and not (isinstance(wh, str) and HEX64.match(wh)):
        errs.append("wasm_hash: must be 64 lowercase hex or null")
    if not _date_ok(rec["deployed_at"]):
        errs.append("deployed_at: must be a valid YYYY-MM-DD date")
    refs = rec["refs"]
    if not isinstance(refs, dict):
        errs.append("refs: must be an object")
    else:
        for k, v in refs.items():
            if not (isinstance(k, str) and ROLE.match(k)):
                errs.append(f"refs key {k!r}: must match [a-z][a-z0-9_]*")
            if not strkey_ok(v, "C"):
                errs.append(f"refs.{k}: not a valid contract strkey: {v!r}")
    if "label" in rec and not _str(rec["label"]):
        errs.append("label: must be a non-empty string")
    if "admin" in rec and not (strkey_ok(rec["admin"], "G") or strkey_ok(rec["admin"], "C")):
        errs.append(f"admin: not a valid account or contract strkey: {rec['admin']!r}")
    if "assets" in rec:
        a = rec["assets"]
        if not (isinstance(a, list) and a and all(strkey_ok(x, "C") for x in a) and len(set(a)) == len(a)):
            errs.append("assets: must be a non-empty list of distinct contract strkeys")
    return errs


def validate_set(records):
    """Cross-record errors: references, wasm hashes, status propagation."""
    errs = []
    contracts = {r["id"]: r for r in records if r["kind"] == "contract"}
    hashes = {r["hash"] for r in records if r["kind"] == "wasm"}
    for r in contracts.values():
        who = f"contract {r['id']} ({r['deployment']}/{r['role']})"
        if r["wasm_hash"] is not None and r["wasm_hash"] not in hashes:
            errs.append(f"{who}: wasm_hash {r['wasm_hash']} has no wasm line")
        for k, target in r["refs"].items():
            if target in contracts:
                if r["status"] == "active" and contracts[target]["status"] == "retired":
                    errs.append(f"{who}: active but refs.{k} {target} is retired")
            elif k not in EXTERNAL_REF_KEYS:
                errs.append(f"{who}: refs.{k} {target} is not in the registry (external ref keys: {sorted(EXTERNAL_REF_KEYS)})")
    return errs


def registry_path(network):
    return DIR / f"{network}.jsonl"


def load(network):
    """Parse a registry file strictly; return (records, errors with file:line)."""
    path = registry_path(network)
    label = f"{path.name}"
    if not path.exists():
        return [], [f"{label}: file missing"]
    # Read bytes strictly: universal-newline decoding would hide a CRLF file
    # that `check` must report as non-canonical. Split on "\n" only, matching
    # the writer — raw U+2028/2029/0085 inside strings are not line breaks.
    text = path.read_bytes().decode("utf-8")
    records, errs, seen, prev = [], [], {}, None
    if text and not text.endswith("\n"):
        errs.append(f"{label}: missing final newline")
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    for n, line in enumerate(lines, 1):
        where = f"{label}:{n}"
        if not line.strip():
            errs.append(f"{where}: blank line")
            continue
        try:
            rec = json.loads(line)
        except ValueError as e:
            errs.append(f"{where}: invalid JSON: {e}")
            continue
        bad = validate_record(rec, network)
        if bad:
            errs += [f"{where}: {m}" for m in bad]
            continue
        if dump(rec) != line:
            errs.append(f"{where}: not canonical (sorted keys, compact separators); run `add`/rewrite to fix")
        k = unique_key(rec)
        if k in seen:
            errs.append(f"{where}: duplicate {k[0]} {k[1]} (first at line {seen[k]})")
        else:
            seen[k] = n
        sk = sort_key(rec)
        if prev is not None and sk < prev:
            errs.append(f"{where}: out of order (lines sort by kind, deployment/artifact, role/hash, id)")
        prev = sk
        records.append(rec)
    if not errs:
        errs += [f"{label}: {m}" for m in validate_set(records)]
    return records, errs


def write(network, records):
    body = "".join(dump(r) + "\n" for r in sorted(records, key=sort_key))
    path = registry_path(network)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent, prefix=path.name, delete=False) as f:
        f.write(body)
    Path(f.name).replace(path)


# ---- commands ---------------------------------------------------------------


def cmd_add(args):
    records, errs = load(args.network)
    if errs:
        return fail(errs)
    src = sys.stdin if args.file == "-" else open(args.file, encoding="utf-8")
    incoming, errs = [], []
    with src:
        for n, line in enumerate(src, 1):
            if not line.strip():
                continue
            try:
                rec = json.loads(line)
            except ValueError as e:
                errs.append(f"{args.file}:{n}: invalid JSON: {e}")
                continue
            errs += [f"{args.file}:{n}: {m}" for m in validate_record(rec, args.network)]
            incoming.append((n, rec))
    if errs:
        return fail(errs)
    by_key = {unique_key(r): r for r in records}
    added = updated = same = 0
    for n, rec in incoming:
        k = unique_key(rec)
        old = by_key.get(k)
        if old is None:
            added += 1
        elif old == rec:
            same += 1
            continue
        elif args.update:
            updated += 1
        else:
            errs.append(f"{args.file}:{n}: conflicts with existing {k[0]} {k[1]} (use --update to replace)")
            continue
        by_key[k] = rec
    if errs:
        return fail(errs)
    merged = list(by_key.values())
    if errs := validate_set(merged):
        return fail(errs)
    write(args.network, merged)
    print(f"{args.network}: {added} added, {updated} updated, {same} unchanged")
    return 0


def cmd_retire(args):
    records, errs = load(args.network)
    if errs:
        return fail(errs)
    hit = [r for r in records if r["kind"] == "contract" and r["deployment"] == args.deployment]
    if not hit:
        return fail([f"{args.network}: no contracts in deployment {args.deployment!r}"])
    changed = 0
    for r in hit:
        if r["status"] != "retired":
            r["status"] = "retired"
            changed += 1
    if errs := validate_set(records):
        return fail(errs)
    write(args.network, records)
    print(f"{args.network}: {args.deployment}: {changed} retired, {len(hit) - changed} already retired")
    return 0


def cmd_check(_args):
    errs = []
    for network in NETWORKS:
        errs += load(network)[1]
    if errs:
        return fail(errs)
    print("registry ok: " + ", ".join(f"{n}={len(load(n)[0])} records" for n in NETWORKS))
    return 0


def rpc(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json", "User-Agent": "precept-registry"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        out = json.load(resp)
    if "error" in out:
        raise RuntimeError(f"{method}: {out['error']}")
    return out["result"]


def _i32(n):
    return n.to_bytes(4, "big")


def _contract_id_bytes(cid):
    return base64.b32decode(cid)[1:33]


def instance_key(cid):
    # LedgerKey::ContractData{ScAddress::Contract(id), ScVal::LedgerKeyContractInstance, Persistent}
    xdr = _i32(6) + _i32(1) + _contract_id_bytes(cid) + _i32(20) + _i32(1)
    return base64.b64encode(xdr).decode()


def code_key(wasm_hash):
    # LedgerKey::ContractCode{hash}
    return base64.b64encode(_i32(7) + bytes.fromhex(wasm_hash)).decode()


def instance_wasm_hash(entry_xdr):
    """Executable WASM hash from a LedgerEntryData::ContractData instance entry, None for a built-in (SAC)."""
    b = base64.b64decode(entry_xdr)
    # type(6) ext(0) addr(1)+32 key(20) durability val(19) executable(0=wasm,1=SAC) [hash]
    if len(b) < 60 or int.from_bytes(b[0:4], "big") != 6 or int.from_bytes(b[44:48], "big") != 20 or int.from_bytes(b[52:56], "big") != 19:
        raise ValueError("unexpected ledger entry layout")
    if int.from_bytes(b[56:60], "big") == 1:
        return None
    return b[60:92].hex()


def fetch(url, keys):
    """key(b64) -> entry xdr(b64) for live entries, batched under the RPC's 200-key limit."""
    found = {}
    for i in range(0, len(keys), 100):
        res = rpc(url, "getLedgerEntries", {"keys": keys[i : i + 100]})
        for e in res.get("entries") or []:
            found[e["key"]] = e["xdr"]
    return found


def cmd_verify(args):
    records, errs = load(args.network)
    if errs:
        return fail(errs)
    active = [r for r in records if r["kind"] == "contract" and r["status"] == "active" and r["wasm_hash"]]
    wasms = [r for r in records if r["kind"] == "wasm"]
    if not active and not wasms:
        print(f"{args.network}: registry empty, nothing to verify")
        return 0
    url = args.rpc or NETWORKS[args.network]
    found = fetch(url, [instance_key(r["id"]) for r in active] + [code_key(r["hash"]) for r in wasms])
    bad = 0
    for r in active:
        tag = f"{r['deployment']}/{r['role']} {r['id']}"
        xdr = found.get(instance_key(r["id"]))
        if xdr is None:
            print(f"FAIL contract {tag}: no live instance (archived or not deployed)")
            bad += 1
            continue
        got = instance_wasm_hash(xdr)
        if got == r["wasm_hash"]:
            print(f"ok   contract {tag} wasm {got[:12]}")
        else:
            print(f"FAIL contract {tag}: on-chain wasm {got}, registry {r['wasm_hash']}")
            bad += 1
    for r in wasms:
        tag = f"{r['artifact']} {r['version']} {r['hash']}"
        if code_key(r["hash"]) in found:
            print(f"ok   wasm     {tag}")
        else:
            print(f"FAIL wasm     {tag}: code not live on {args.network} (archived or never uploaded)")
            bad += 1
    print(f"{args.network}: {len(active)} active contracts, {len(wasms)} wasm hashes, {bad} failures")
    return 1 if bad else 0


def fail(errs):
    for e in errs:
        print(e, file=sys.stderr)
    return 1


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("add", "retire", "verify"):
        s = sub.add_parser(name)
        s.add_argument("--network", required=True, choices=sorted(NETWORKS))
        if name == "add":
            s.add_argument("file", metavar="FILE.jsonl|-")
            s.add_argument("--update", action="store_true", help="replace conflicting existing records")
        elif name == "retire":
            s.add_argument("--deployment", required=True)
        else:
            s.add_argument("--rpc", help="Soroban JSON-RPC URL (default: the network's public RPC)")
    sub.add_parser("check")
    args = p.parse_args(argv)
    return {"add": cmd_add, "retire": cmd_retire, "check": cmd_check, "verify": cmd_verify}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
