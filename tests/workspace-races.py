#!/usr/bin/env python3
"""Regression tests for the five workspace review findings. All state is isolated."""

import importlib.util
import json
from pathlib import Path
import sys
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "workspace_contract_fixture", Path(__file__).with_name("workspace-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class WorkspaceRaces(unittest.TestCase):
    # Composition avoids collecting the inherited workspace contract tests again.
    def setUp(self):
        self.case = fixture.WorkspaceContract()
        self.case.setUp()

    def tearDown(self):
        self.case.tearDown()

    def test_worker_steer_delivers_output_before_acknowledgement(self):
        t = self.case
        lead = t.start(t.lead())
        worker = t.start(t.worker(lead))
        server = t.runtime.server
        original_call = server.call
        original_wait = server.wait
        output_delivered = threading.Event()

        def call(method, params, timeout=60):
            if method == "turn/steer":
                # FakeServer.submit uses another thread, as the protocol reader does.
                server.notify(
                    {
                        "method": "item/agentMessage/delta",
                        "params": {
                            "threadId": worker["threadId"],
                            "turnId": worker["turnId"],
                            "itemId": "steer-progress",
                            "delta": "Progress before acknowledgement",
                        },
                    }
                )
                output_delivered.set()
            return original_call(method, params, timeout)

        with patch.object(server, "call", side_effect=call), patch.object(
            server,
            "wait",
            side_effect=lambda future, timeout=60: original_wait(
                future, min(timeout, 1)
            ),
        ):
            response = t.tool(
                lead,
                "orchestration_send",
                {
                    "agent_id": worker["id"],
                    "text": "Correct the current task",
                    "delivery": "steer",
                },
            )
        self.assertTrue(response["success"], response)
        self.assertTrue(output_delivered.wait(1))
        self.assertEqual(
            json.loads(response["contentItems"][0]["text"])["status"], "delivered"
        )
        self.assertTrue(
            any(
                "Progress before acknowledgement" in item["text"]
                for item in t.runtime.transcript(worker["id"])["items"]
            )
        )

    def test_pause_or_delete_during_prepare_prevents_rule_command(self):
        t = self.case
        lead = t.runtime.prepare(t.lead())
        t.agent_update(lead, approvalPolicy="never")
        for action in ("pause", "delete"):
            with self.subTest(action=action):
                rule = t.runtime.rules(
                    {"agent": lead["id"], "name": action, "command": "held-rule-check"}
                )
                rule.update(inFlight=True, checks=1)
                with t.runtime.lock, t.runtime.db() as db:
                    t.runtime.put(db, "rules", rule)
                entered, release = threading.Event(), threading.Event()
                original_prepare = t.runtime.prepare

                def prepare(agent):
                    entered.set()
                    if not release.wait(3):
                        raise RuntimeError("Test preparation gate timed out")
                    return original_prepare(agent)

                with patch.object(t.runtime, "prepare", side_effect=prepare):
                    thread = threading.Thread(target=t.runtime.run_rule, args=(rule,))
                    thread.start()
                    try:
                        self.assertTrue(entered.wait(2))
                        t.runtime.rules(
                            {"action": action, "agent": lead["id"], "id": rule["id"]}
                        )
                    finally:
                        release.set()
                        thread.join(3)
                self.assertFalse(thread.is_alive())
                self.assertFalse(
                    any(
                        m.get("ruleId") == rule["id"]
                        and m["status"] in {"starting", "running", "approval"}
                        for m in t.runtime.snapshot()["monitors"]
                    )
                )
                self.assertFalse(
                    any(
                        method == "command/exec" for method, _ in t.runtime.server.calls
                    )
                )
                if action == "pause":
                    self.assertFalse(t.rule_record(rule["id"])["inFlight"])

    def test_restore_excludes_new_monitor_until_file_restore_finishes(self):
        t = self.case
        worker, path = t.isolated_worker()
        worker = t.runtime.prepare(worker)
        peer = t.worker(
            t.runtime.agent(worker["rootId"]), "Same workspace", cwd=str(path)
        )
        t.agent_update(worker, lastCompletedTurn="turn-a")
        checkpoint = t.runtime.checkpoint_capture(worker["id"])
        (path / "tracked.txt").write_text("changed\n")
        preview = t.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        entered, release = threading.Event(), threading.Event()
        original_call = t.runtime.server.call
        outcomes, failures = [], []

        def call(method, params, timeout=60):
            if method == "thread/fork":
                entered.set()
                if not release.wait(3):
                    raise RuntimeError("Test fork gate timed out")
            return original_call(method, params, timeout)

        def restore():
            try:
                outcomes.append(
                    t.runtime.restore_checkpoint(
                        worker["id"],
                        {
                            "checkpoint": checkpoint["id"],
                            "expectedTree": preview["expectedTree"],
                        },
                    )
                )
            except Exception as error:
                failures.append(error)

        with patch.object(t.runtime.server, "call", side_effect=call):
            thread = threading.Thread(target=restore)
            thread.start()
            try:
                self.assertTrue(entered.wait(2))
                for agent in (worker, peer):
                    with self.subTest(agent=agent["name"]), self.assertRaisesRegex(
                        ValueError, "[Ww]orkspace"
                    ):
                        t.runtime.monitor(
                            agent["id"], {"command": "held-rule-check"}, approved=True
                        )
                try:
                    t.runtime.send(peer["id"], "Modify the shared workspace")
                except ValueError as error:
                    self.assertIn("workspace", str(error).lower())
                t.runtime.dispatch()
                self.assertFalse(t.runtime.agent(peer["id"])["inFlight"])
            finally:
                release.set()
                thread.join(3)
        self.assertFalse(thread.is_alive())
        self.assertEqual(failures, [])
        self.assertEqual(outcomes[0]["status"], "restored")
        self.assertEqual((path / "tracked.txt").read_text(), "base\n")
        self.assertFalse(
            any(method == "command/exec" for method, _ in t.runtime.server.calls)
        )

    def test_recovery_restore_recovers_transcript_and_search(self):
        t = self.case
        worker, _ = t.isolated_worker()
        t.agent_update(worker, threadId="original-thread", lastCompletedTurn="turn-a")
        with t.runtime.lock, t.runtime.db() as db:
            t.runtime.item(
                db, worker["id"], "answer-a", "assistant", "Answer A", turnId="turn-a"
            )
        first = t.runtime.checkpoint_capture(worker["id"])
        with t.runtime.lock, t.runtime.db() as db:
            t.runtime.item(
                db,
                worker["id"],
                "answer-b",
                "assistant",
                "recoverablecanary",
                turnId="turn-b",
            )
        t.agent_update(worker, lastCompletedTurn="turn-b")

        def restore(checkpoint):
            preview = t.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
            t.runtime.restore_checkpoint(
                worker["id"],
                {
                    "checkpoint": checkpoint["id"],
                    "expectedTree": preview["expectedTree"],
                },
            )

        restore(first)
        self.assertEqual(t.runtime.search_work("recoverablecanary")["results"], [])
        with t.runtime.db() as db:
            recovery = next(
                c
                for c in t.runtime.records(db, "checkpoints")
                if c["label"] == "Before restore"
            )
        restore(recovery)
        self.assertEqual(t.runtime.agent(worker["id"])["lastCompletedTurn"], "turn-b")
        self.assertTrue(
            any(
                item["text"] == "recoverablecanary"
                for item in t.runtime.transcript(worker["id"])["items"]
            )
        )
        self.assertTrue(t.runtime.search_work("recoverablecanary")["results"])

    def test_declined_rule_command_releases_check_and_allows_next_due_tick(self):
        t = self.case
        lead = t.lead()
        rule = t.runtime.rules(
            {"agent": lead["id"], "name": "Approval", "command": "fixture-rule-command"}
        )
        t.due(rule)
        fixture.eventually(lambda: bool(t.runtime.snapshot()["requests"]))
        request = t.runtime.snapshot()["requests"][0]
        t.runtime.answer(request["id"], {"decision": "decline"})
        self.assertFalse(t.rule_record(rule["id"])["inFlight"])
        self.assertEqual(t.events(lead, "rule"), [])
        self.assertEqual(t.events(lead, "monitor_cancelled"), [])
        t.due(rule)
        fixture.eventually(lambda: t.rule_record(rule["id"])["checks"] == 2)
        fixture.eventually(lambda: bool(t.runtime.snapshot()["requests"]))
        self.assertNotEqual(t.runtime.snapshot()["requests"][0]["id"], request["id"])
        self.assertFalse(
            any(method == "turn/start" for method, _ in t.runtime.server.calls)
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
