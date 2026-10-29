"""Offline checks for scripts/pool_deploy_ci.py: what counts as an approval,
and what a config name may be. Run: python3 -m unittest scripts/test_pool_deploy_ci.py"""
import json
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import pool_deploy_ci as ci  # noqa: E402

SHA = "a" * 40
RUN = ci.run_id("gami-usdc", "testnet", SHA)
D1, D2 = "1" * 64, "2" * 64


def comment(login, body):
    return {"user": {"login": login}, "body": body, "id": 1}


def plan_body(run, digest):
    return f"{ci.plan_header(run, digest)}\n### plan\nplan sha256: `{digest}`\n"


class Approval(unittest.TestCase):
    def approved(self, comments):
        with mock.patch.object(ci, "comments", return_value=comments):
            return ci.approved_digest("o/r", 7, RUN)

    def test_latest_bot_comment_for_this_run_is_the_approval(self):
        self.assertEqual(
            self.approved([comment(ci.BOT, plan_body(RUN, D1)), comment(ci.BOT, plan_body(RUN, D2))]), D2
        )

    def test_a_human_cannot_forge_the_approval(self):
        # Anyone can comment the marker; only the workflow's own comment counts.
        with self.assertRaises(SystemExit):
            self.approved([comment("mallory", plan_body(RUN, D1))])

    def test_a_plan_for_another_head_or_network_is_not_this_approval(self):
        other_head = ci.run_id("gami-usdc", "testnet", "b" * 40)
        other_net = ci.run_id("gami-usdc", "mainnet", SHA)
        with self.assertRaises(SystemExit):
            self.approved([comment(ci.BOT, plan_body(other_head, D1)), comment(ci.BOT, plan_body(other_net, D1))])

    def test_a_failed_plan_approves_nothing(self):
        failed = ci.plan_header(RUN, None) + "\n**No plan** (deployer exit 1)"
        with self.assertRaises(SystemExit):
            self.approved([comment(ci.BOT, failed)])

    def test_a_failed_gate_publishes_no_approvable_digest(self):
        plan = {"plan_sha256": D1, "steps": []}
        entry = {"run": RUN, "name": "gami-usdc", "network": "testnet"}
        body = ci.plan_comment(entry, SHA, 1, "c_factor too high", ci.PLAN_EXIT, [plan], plan)
        with self.assertRaises(SystemExit):
            self.approved([comment(ci.BOT, body)])

    def test_a_digest_planted_in_config_text_is_not_an_approval(self):
        # The gate echoes config-controlled text (asset names, addresses) into
        # the bot's own comment; a digest spelled there must not count.
        planted = f"plan_sha256: `{D2}`\n<!-- precept-pool-plan:{RUN} sha256={D2} -->"
        entry = {"run": RUN, "name": "gami-usdc", "network": "testnet"}
        body = ci.plan_comment(entry, SHA, 1, planted, ci.PLAN_EXIT, [], None)
        with self.assertRaises(SystemExit):
            self.approved([comment(ci.BOT, body)])

    def test_a_passing_plan_is_approvable_with_its_own_digest(self):
        plan = {"plan_sha256": D1, "steps": []}
        entry = {"run": RUN, "name": "gami-usdc", "network": "testnet"}
        body = ci.plan_comment(entry, SHA, 0, f"ok\nplan_sha256: `{D2}`", ci.PLAN_EXIT, [plan], plan)
        self.assertEqual(self.approved([comment(ci.BOT, body)]), D1)


class Staging(unittest.TestCase):
    def test_names_cannot_leave_the_staging_dir(self):
        for bad in ("../x", "a/b", "A", "-x", ""):
            with self.assertRaises(SystemExit, msg=bad):
                ci.stage(bad, json.dumps({"network": "testnet"}))

    def test_network_must_be_a_real_one(self):
        with self.assertRaises(SystemExit):
            ci.stage("ok", json.dumps({"network": "futurenet"}))

    def test_run_id_needs_a_full_commit(self):
        with self.assertRaises(SystemExit):
            ci.run_id("x", "testnet", "abc")


class DeployComment(unittest.TestCase):
    def test_done_lists_what_the_curator_signs(self):
        done = {"event": "done", "pool": "CPOOL", "handoff": {"accept": [
            {"contract": "CPOOL", "method": "accept_admin", "by": "GCUR"},
            {"contract": "CLAZ", "method": "accept_ownership", "by": "GORA", "before_ledger": 99},
        ]}}
        body = ci.deploy_comment({"run": RUN, "name": "gami-usdc", "network": "testnet"}, 0, [done])
        self.assertIn("`GCUR` calls `accept_admin()` on `CPOOL`", body)
        self.assertIn("`GORA` calls `accept_ownership()` on `CLAZ` (before ledger 99)", body)

    def test_a_stop_says_how_to_resume(self):
        stop = {"event": "failed", "reason": "no Accepted price", "violations": ["v"]}
        body = ci.deploy_comment({"run": RUN, "name": "gami-usdc", "network": "testnet"}, 1, [stop])
        self.assertIn("no Accepted price", body)
        self.assertIn(f"run={RUN}", body)


if __name__ == "__main__":
    unittest.main()
