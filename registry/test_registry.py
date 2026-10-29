"""Offline checks for registry/registry.py. `python3 -m unittest discover -s registry`."""
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout, redirect_stderr
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import registry  # noqa: E402


def contract(**over):
    rec = {
        "kind": "contract",
        "network": "testnet",
        "id": registry.strkey_encode("C", b"\x11" * 32),
        "role": "pool",
        "deployment": "demo",
        "status": "active",
        "wasm_hash": "a" * 64,
        "refs": {"pool_factory": registry.strkey_encode("C", b"\x22" * 32)},
        "deployed_at": "2026-10-25",
    }
    rec.update(over)
    return rec


def factory(**over):
    rec = contract(
        id=registry.strkey_encode("C", b"\x22" * 32),
        role="pool_factory",
        refs={},
        label="factory",
        admin=registry.strkey_encode("G", b"\x33" * 32),
        assets=[registry.strkey_encode("C", b"\x44" * 32)],
    )
    rec.update(over)
    return rec


def wasm(**over):
    rec = {
        "kind": "wasm",
        "network": "testnet",
        "hash": "a" * 64,
        "artifact": "pool",
        "version": "v0.1.1",
        "source": {"repo": "Templar-Protocol/precept", "tag": "v0.1.1", "asset": "pool.wasm"},
    }
    rec.update(over)
    return rec


def bare(**over):
    """A pool record with no refs and no wasm line requirement (SAC-like)."""
    return contract(refs={}, wasm_hash=None, **over)


class RegistryTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        registry.DIR = self.dir
        self.fetch, self.rpc = registry.fetch, registry.rpc
        for net in registry.NETWORKS:
            self.write_lines(net, [])

    def tearDown(self):
        registry.DIR = None
        registry.fetch, registry.rpc = self.fetch, self.rpc
        self.tmp.cleanup()

    def write_lines(self, net, recs):
        body = "".join(registry.dump(r) + "\n" for r in sorted(recs, key=registry.sort_key))
        (self.dir / f"{net}.jsonl").write_text(body, encoding="utf-8")

    def run_cli(self, *argv):
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(out):
            code = registry.main(list(argv))
        return code, out.getvalue()

    def seed(self, *recs):
        self.write_lines("testnet", recs)

    def read(self, net="testnet"):
        text = (self.dir / f"{net}.jsonl").read_text(encoding="utf-8")
        return [json.loads(l) for l in text.split("\n")[:-1]]

    def assertClean(self, net="testnet"):
        errs = registry.load(net)[1]
        self.assertEqual(errs, [], "\n".join(errs))

    # ---- add ---------------------------------------------------------------

    def test_line_separators_in_strings_roundtrip(self):
        # U+2028/2029/0085 are raw in canonical output (ensure_ascii=False) and
        # must not be treated as line breaks when the file is read back.
        rec = bare(label="a\u2028b\u2029c\u0085d")
        code, out = self.run_cli("add", "--network", "testnet", self.input_file([rec]))
        self.assertEqual(code, 0, out)
        self.assertClean()
        self.assertEqual(self.read(), [rec])
        code, out = self.run_cli("check")
        self.assertEqual(code, 0, out)

    def test_crlf_file_is_not_canonical(self):
        (self.dir / "testnet.jsonl").write_bytes((registry.dump(bare()) + "\r\n").encode())
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("not canonical", out)

    def test_add_writes_canonical_order(self):
        code, out = self.run_cli("add", "--network", "testnet", self.input_file([factory(), contract(), wasm()]))
        self.assertEqual((code, out.strip()), (0, "testnet: 3 added, 0 updated, 0 unchanged"))
        lines = (self.dir / "testnet.jsonl").read_text(encoding="utf-8").split("\n")[:-1]
        self.assertEqual([json.loads(l)["kind"] for l in lines], ["contract", "contract", "wasm"])
        self.assertEqual(lines, [registry.dump(r) for r in (contract(), factory(), wasm())])
        self.assertTrue((self.dir / "testnet.jsonl").read_text().endswith("\n"))
        self.assertClean()

    def test_add_stdin_and_noop_readd(self):
        real_stdin, sys.stdin = sys.stdin, io.StringIO(registry.dump(bare()) + "\n\n")
        try:
            code, out = self.run_cli("add", "--network", "testnet", "-")
        finally:
            sys.stdin = real_stdin
        self.assertEqual(code, 0)
        self.assertEqual(self.read(), [bare()])
        code, out = self.run_cli("add", "--network", "testnet", self.input_file([bare()]))
        self.assertEqual((code, out.strip()), (0, "testnet: 0 added, 0 updated, 1 unchanged"))
        self.assertEqual(self.read(), [bare()])

    def test_add_conflict_needs_update(self):
        self.seed(bare())
        changed = bare(deployed_at="2026-10-26")
        path = self.input_file([changed])
        code, out = self.run_cli("add", "--network", "testnet", path)
        self.assertEqual(code, 1)
        self.assertIn("conflicts with existing contract", out)
        self.assertEqual(self.read(), [bare()])
        code, out = self.run_cli("add", "--network", "testnet", path, "--update")
        self.assertEqual(code, 0)
        self.assertEqual(self.read(), [changed])

    def test_add_rejects_bad_record_and_dangling_ref(self):
        code, out = self.run_cli("add", "--network", "testnet", self.input_file([contract(wasm_hash="b" * 64)]))
        self.assertEqual(code, 1)
        self.assertIn("has no wasm line", out)
        self.assertFalse((self.dir / "testnet.jsonl").read_text())
        code, out = self.run_cli("add", "--network", "testnet", self.input_file([contract(admin="G" + "A" * 55)]))
        self.assertEqual(code, 1)
        self.assertIn("admin: not a valid account or contract strkey", out)

    def test_admin_may_be_account_or_contract(self):
        # A multisig smart account (C…) may administer a contract, not just a G… key.
        smart = registry.strkey_encode("C", b"\x66" * 32)
        self.seed(contract(admin=smart), factory(), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 0, out)
        broken = smart[:-1] + ("A" if smart[-1] != "A" else "B")
        self.seed(contract(admin=broken), factory(), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("admin: not a valid account or contract strkey", out)

    def input_file(self, recs):
        p = self.dir / "in.jsonl"
        p.write_text("".join(registry.dump(r) + "\n" for r in recs), encoding="utf-8")
        return str(p)

    # ---- retire ------------------------------------------------------------

    def test_retire(self):
        self.seed(contract(), factory(), wasm())
        code, out = self.run_cli("retire", "--network", "testnet", "--deployment", "demo")
        self.assertEqual(code, 0)
        self.assertIn("2 retired", out)
        self.assertEqual([r.get("status") for r in self.read()], ["retired", "retired", None])
        code, out = self.run_cli("retire", "--network", "testnet", "--deployment", "demo")
        self.assertEqual(code, 0)
        self.assertIn("already retired", out)
        code, out = self.run_cli("retire", "--network", "testnet", "--deployment", "nope")
        self.assertEqual(code, 1)
        self.assertIn("no contracts in deployment", out)

    def test_retire_blocked_by_active_referrer(self):
        self.seed(contract(), factory(), wasm())
        self.write_lines("testnet", [
            contract(id=registry.strkey_encode("C", b"\x77" * 32), deployment="other", refs={"pool": contract()["id"]}), contract(), factory(), wasm()])
        code, out = self.run_cli("retire", "--network", "testnet", "--deployment", "demo")
        self.assertEqual(code, 1)
        self.assertIn("is retired", out)

    # ---- check -------------------------------------------------------------

    def test_check_ok(self):
        self.seed(contract(), factory(), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 0, out)
        self.assertIn("registry ok", out)

    def test_check_bad_strkey_checksum(self):
        bad = contract(id=contract()["id"][:-1] + ("A" if contract()["id"][-1] != "A" else "B"))
        self.seed(bad, factory(), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("id: not a valid contract strkey", out)

    def test_check_bad_hex(self):
        self.seed(contract(wasm_hash="A" * 64), factory(), wasm(hash="A" * 64))
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("wasm_hash: must be 64 lowercase hex", out)

    def test_check_unsorted(self):
        (self.dir / "testnet.jsonl").write_text(
            registry.dump(factory()) + "\n" + registry.dump(contract()) + "\n" + registry.dump(wasm()) + "\n")
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("out of order", out)

    def test_check_duplicate(self):
        (self.dir / "testnet.jsonl").write_text(
            registry.dump(contract()) + "\n" + registry.dump(contract()) + "\n" + registry.dump(factory()) + "\n" + registry.dump(wasm()) + "\n")
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("duplicate contract", out)

    def test_check_dangling_ref_and_wasm(self):
        self.seed(contract(refs={"backstop": registry.strkey_encode("C", b"\x55" * 32)}), factory(), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("refs.backstop", out)
        self.assertIn("not in the registry", out)
        self.seed(contract(), factory(refs={"token": registry.strkey_encode("C", b"\x55" * 32)}), wasm())
        self.assertClean()  # external ref keys are allowed

    def test_check_bad_status(self):
        self.seed(contract(status="paused"), factory(), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("status: must be one of", out)

    def test_check_non_canonical_and_missing_final_newline(self):
        body = registry.dump(contract()).replace('","', '", "') + "\n" + registry.dump(factory()) + "\n" + registry.dump(wasm())
        (self.dir / "testnet.jsonl").write_text(body)
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("not canonical", out)
        self.assertIn("missing final newline", out)

    def test_check_wrong_network_field(self):
        self.seed(contract(), factory(network="mainnet"), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("in a testnet file", out)

    def test_check_bad_dates_and_source(self):
        self.seed(contract(deployed_at="2026-13-01"), factory(), wasm())
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("deployed_at", out)
        self.seed(contract(), factory(), wasm(source={"repo": "x", "tag": "v1"}))
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("tag and asset go together", out)
        self.seed(contract(), factory(), wasm(source={"repo": "x", "commit": "abc"}))
        code, out = self.run_cli("check")
        self.assertEqual(code, 1)
        self.assertIn("source.commit", out)

    # ---- verify ------------------------------------------------------------

    def test_verify_uses_rpc_and_reports_mismatches(self):
        self.seed(contract(), factory(wasm_hash=None), wasm())
        calls = []

        def fake_fetch(url, keys):
            calls.append(url)
            out = {}
            for k in keys:
                if k == registry.instance_key(contract()["id"]):
                    out[k] = self.instance_xdr("b" * 64)
                elif k == registry.code_key("a" * 64):
                    out[k] = "x"
            return out

        registry.fetch = fake_fetch
        code, out = self.run_cli("verify", "--network", "testnet")
        self.assertEqual(code, 1)
        self.assertIn("https://soroban-testnet.stellar.org", calls)
        self.assertIn("FAIL contract demo/pool", out)
        self.assertIn("on-chain wasm " + "b" * 64, out)
        self.assertIn("ok   wasm", out)

    def test_verify_ok(self):
        self.seed(contract(), factory(wasm_hash=None), wasm())
        registry.fetch = lambda url, keys: {
            k: (self.instance_xdr("a" * 64) if k == registry.instance_key(contract()["id"]) else "x") for k in keys
        }
        code, out = self.run_cli("verify", "--network", "testnet", "--rpc", "http://rpc")
        self.assertEqual(code, 0, out)
        self.assertIn("0 failures", out)

    def test_verify_missing_code(self):
        self.seed(contract(), factory(wasm_hash=None), wasm())
        iid = registry.instance_key(contract()["id"])
        registry.fetch = lambda url, keys: {k: self.instance_xdr("a" * 64) for k in keys if k == iid}
        code, out = self.run_cli("verify", "--network", "testnet")
        self.assertEqual(code, 1)
        self.assertIn("FAIL wasm", out)
        self.assertIn("code not live", out)
        self.assertIn("ok   contract demo/pool", out)

    def test_verify_no_live_instance(self):
        self.seed(contract(), factory(wasm_hash=None), wasm())
        code_k = registry.code_key("a" * 64)
        registry.fetch = lambda url, keys: {k: "x" for k in keys if k == code_k}
        code, out = self.run_cli("verify", "--network", "testnet")
        self.assertEqual(code, 1)
        self.assertIn("no live instance", out)
        self.assertIn("ok   wasm", out)

    def test_fetch_batches_keys_by_100(self):
        sizes = []

        def fake_rpc(url, method, params):
            self.assertEqual((url, method), ("http://rpc", "getLedgerEntries"))
            sizes.append(len(params["keys"]))
            if len(params["keys"]) == 50:
                return {}  # no "entries" at all must be tolerated
            return {"entries": [{"key": params["keys"][0], "xdr": "x"}]}

        registry.rpc = fake_rpc
        keys = [str(i) for i in range(250)]
        found = registry.fetch("http://rpc", keys)
        self.assertEqual(sizes, [100, 100, 50])
        self.assertEqual(set(found), {"0", "100"})

    def test_verify_mainnet_uses_public_rpc_unless_overridden(self):
        (self.dir / "mainnet.jsonl").write_text(registry.dump(wasm(network="mainnet")) + "\n")
        urls = []
        registry.fetch = lambda url, keys: urls.append(url) or {k: "x" for k in keys}
        self.run_cli("verify", "--network", "mainnet")
        self.run_cli("verify", "--network", "mainnet", "--rpc", "http://own")
        self.assertEqual(urls, [registry.NETWORKS["mainnet"], "http://own"])

    def test_verify_empty_is_noop(self):
        code, out = self.run_cli("verify", "--network", "testnet")
        self.assertEqual(code, 0)
        self.assertIn("nothing to verify", out)

    def test_instance_xdr_layout(self):
        b = bytearray(60 + 32)
        b[0:4] = (6).to_bytes(4, "big")
        b[44:48] = (20).to_bytes(4, "big")
        b[52:56] = (19).to_bytes(4, "big")
        b[56:60] = (0).to_bytes(4, "big")
        b[60:] = bytes.fromhex("c" * 64)
        xdr = registry.base64.b64encode(bytes(b)).decode()
        self.assertEqual(registry.instance_wasm_hash(xdr), "c" * 64)
        b[56:60] = (1).to_bytes(4, "big")
        self.assertIsNone(registry.instance_wasm_hash(registry.base64.b64encode(bytes(b[:60])).decode()))
        with self.assertRaises(ValueError):
            registry.instance_wasm_hash(registry.base64.b64encode(bytes(64)).decode())

    def instance_xdr(self, wasm_hash):
        b = bytearray(60 + 32)
        b[0:4] = (6).to_bytes(4, "big")
        b[44:48] = (20).to_bytes(4, "big")
        b[52:56] = (19).to_bytes(4, "big")
        b[56:60] = (0).to_bytes(4, "big")
        b[60:] = bytes.fromhex(wasm_hash)
        return registry.base64.b64encode(bytes(b)).decode()


G_ADMIN = registry.strkey_encode("G", b"\x55" * 32)
C_ADMIN = registry.strkey_encode("C", b"\x56" * 32)


def reserve(index, label, **over):
    c = {
        "index": index, "decimals": 7, "c_factor": 0, "l_factor": registry.SCALE,
        "util": 9_000_000, "max_util": 9_900_000, "r_base": 1000, "r_one": 0,
        "r_two": 0, "r_three": 0, "reactivity": 0,
        "supply_cap": str(registry.I128_MAX), "min_position": "10000000", "enabled": True,
    }
    c.update(over)
    return {"asset": registry.strkey_encode("C", bytes([0x60 + index]) * 32), "label": label, "config": c}


def oracle_asset(**over):
    a = {
        "min_sources": 1, "max_age_secs": 3600,
        "sources": [{"kind": "sep40", "contract": registry.strkey_encode("C", b"\x99" * 32),
                     "asset": {"Other": "7"}}],
    }
    a.update(over)
    return a


def pool_cfg(**over):
    """A config that passes check-config; `over` replaces a top-level key."""
    cfg = {
        "name": "test-pool",
        "network": "mainnet",
        "pool": {"admin": G_ADMIN, "name": "Test", "oracle": None,
                 "backstop_take_rate": 0, "max_positions": 4, "min_collateral": "0"},
        "reserves": [reserve(0, "usdc"),
                     reserve(1, "xlm", c_factor=8_000_000, l_factor=0, util=8_000_000,
                             max_util=9_000_000, supply_cap="200000000000000",
                             min_position="500000000")],
        "target_status": 0,
        "oracle": {"assets": {"usdc": oracle_asset(), "xlm": oracle_asset()}},
    }
    cfg.update(over)
    return cfg


def setp(obj, dotted, value):
    node = obj
    parts = dotted.split(".")
    for p in parts[:-1]:
        node = node[int(p)] if isinstance(node, list) else node[p]
    last = parts[-1]
    if isinstance(node, list):
        node[int(last)] = value
    else:
        node[last] = value
    return obj


class CheckConfigTest(unittest.TestCase):
    """`registry.py check-config`, offline: the RPC lookup is mocked."""

    def setUp(self):
        self.fetch = registry.fetch
        registry.fetch = lambda url, keys: {k: "x" for k in keys}

    def tearDown(self):
        registry.fetch = self.fetch

    def problems(self, *edits, network="mainnet"):
        cfg = pool_cfg()
        for dotted, value in edits:
            setp(cfg, dotted, value)
        return registry.check_config(cfg, network, "http://rpc")[0]

    def assertProblem(self, errs, needle):
        self.assertTrue(any(needle in e for e in errs), f"{needle!r} not in {errs}")

    def test_clean_config_passes(self):
        self.assertEqual(self.problems(), [])

    def test_real_repo_config_passes(self):
        path = Path(__file__).resolve().parent / "configs" / "xlm-usdc.json"
        errs, summary = registry.check_config(json.loads(path.read_text()), "mainnet", "http://rpc")
        self.assertEqual(errs, [], "\n".join(errs))
        self.assertIn("reuses runtime", summary)

    def test_wrong_network(self):
        self.assertProblem(self.problems(("network", "testnet"), ), "expected 'mainnet'")
        self.assertProblem(self.problems(network="testnet"), "expected 'testnet'")

    def test_c_factor_bound(self):
        self.assertProblem(self.problems(("reserves.1.config.c_factor", registry.SCALE)),
                           "reserves[1].config.c_factor")

    def test_l_factor_bound(self):
        self.assertProblem(self.problems(("reserves.0.config.l_factor", registry.SCALE + 1)),
                           "reserves[0].config.l_factor")

    def test_util_bounds(self):
        self.assertProblem(self.problems(("reserves.0.config.util", 0)), "0 < util < max_util")
        self.assertProblem(self.problems(("reserves.0.config.util", 9_900_000)), "0 < util < max_util")
        self.assertProblem(self.problems(("reserves.0.config.max_util", registry.SCALE)), "0 < util < max_util")
        # A collateral-only reserve (l_factor 0) is exempt.
        self.assertEqual(self.problems(("reserves.1.config.util", 0)), [])

    def test_i128_fields(self):
        for field in ("supply_cap", "min_position"):
            for bad in ("0", "-1", "1e9", "", "1" * 40):
                self.assertProblem(self.problems((f"reserves.0.config.{field}", bad)), field)
        self.assertEqual(self.problems(("reserves.0.config.supply_cap", str(registry.I128_MAX))), [])

    def test_oracle_quorum_and_age(self):
        self.assertProblem(self.problems(("oracle.assets.usdc.min_sources", 0)), "min_sources")
        self.assertProblem(self.problems(("oracle.assets.usdc.min_sources", 2)), "min_sources")
        self.assertProblem(self.problems(("oracle.assets.usdc.max_age_secs", 30)), "max_age_secs")
        self.assertProblem(self.problems(("oracle.assets.usdc.max_age_secs", 86_401)), "max_age_secs")

    def test_reserve_without_oracle_entry(self):
        self.assertProblem(self.problems(("reserves.1.label", "gone")), "oracle.assets.gone: missing")

    def test_backstop_take_rate_bound(self):
        self.assertProblem(self.problems(("pool.backstop_take_rate", registry.SCALE + 1)),
                           "pool.backstop_take_rate")

    def test_max_positions_bound(self):
        for bad in (0, 31):
            self.assertProblem(self.problems(("pool.max_positions", bad)), "pool.max_positions")

    def test_reserve_indices_must_be_in_order(self):
        self.assertProblem(self.problems(("reserves.1.config.index", 5)), "0..n-1 in order")

    def test_undecided_null_is_an_error(self):
        self.assertProblem(self.problems(("pool.max_positions", None)), "undecided")
        # pool.oracle is filled by the deployer, so its null stays fine.
        self.assertEqual(self.problems(("pool.oracle", None)), [])

    def test_missing_admin_account(self):
        registry.fetch = lambda url, keys: {}
        self.assertProblem(self.problems(), f"{G_ADMIN} has no account on mainnet")
        self.assertProblem(self.problems(("pool.admin", C_ADMIN)),
                           f"{C_ADMIN} has no contract instance on mainnet")

    def test_malformed_admin(self):
        # Recorded rather than failing inside fetch: check_config turns a
        # fetch exception into a problem line, so self.fail() there would pass.
        calls = []
        registry.fetch = lambda url, keys: calls.append(keys) or {}
        for bad in ("G" + "A" * 55, "X" + G_ADMIN[1:], 56 * "G"):
            errs = self.problems(("pool.admin", bad))
            self.assertProblem(errs, "not a checksum-valid G or C strkey")
        self.assertEqual(calls, [], "a malformed admin must not be looked up on chain")

    def test_missing_sources_with_a_bad_quorum_reports_instead_of_crashing(self):
        errs = self.problems(("oracle.assets.usdc.sources", None), ("oracle.assets.usdc.min_sources", 0))
        self.assertProblem(errs, "oracle.assets.usdc.min_sources")

    def test_optional_oracle_admin(self):
        self.assertEqual(self.problems(("oracle.admin", C_ADMIN)), [])
        registry.fetch = lambda url, keys: {}
        self.assertProblem(self.problems(("oracle.admin", C_ADMIN)), "oracle.admin")

    def test_own_runtime_summary(self):
        cfg = pool_cfg()
        self.assertIn("deploys its own runtime", registry.check_config(cfg, "mainnet", "http://rpc")[1])
        self.assertIn("reuses runtime", registry.check_config(setp(pool_cfg(), "oracle.runtime", C_ADMIN),
                                                              "mainnet", "http://rpc")[1])

    def test_cli_exit_code_and_rpc_default(self):
        d = Path(tempfile.mkdtemp())
        good, bad = d / "good.json", d / "bad.json"
        good.write_text(json.dumps(pool_cfg()))
        bad.write_text(json.dumps(setp(pool_cfg(), "pool.max_positions", 99)))
        urls = []
        registry.fetch = lambda url, keys: urls.append(url) or {k: "x" for k in keys}
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(out):
            code = registry.main(["check-config", str(good), "--network", "mainnet"])
        self.assertEqual(code, 0, out.getvalue())
        self.assertIn(f"{good}: ok", out.getvalue())
        self.assertEqual(urls, [registry.NETWORKS["mainnet"]])
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(out):
            code = registry.main(["check-config", str(good), str(bad), "--network", "mainnet",
                                  "--rpc", "http://own"])
        self.assertEqual(code, 1)
        self.assertIn(f"{bad}: pool.max_positions", out.getvalue())
        self.assertEqual(urls[-1], "http://own")


if __name__ == "__main__":
    unittest.main()
