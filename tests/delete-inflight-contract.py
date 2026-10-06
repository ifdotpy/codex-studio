#!/usr/bin/env python3
"""Deleting a conversation releases its active native turn slot."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "preparation_writer_fixture", Path(__file__).with_name("preparation-writer-lock-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class DeleteInflightContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-delete-inflight-")
        self.addCleanup(self.temp.cleanup)
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.FakeServer)
        self.addCleanup(self.runtime.close)

    def slots(self):
        with self.runtime.read_db() as db:
            return self.runtime.dispatch_active_slots(db)

    def running_chat(self):
        agent = self.runtime.create(
            {"name": "Delete while running", "cwd": self.temp.name, "prompt": "Start"},
            defer=True,
        )
        baseline = len(self.slots())
        self.runtime.send(agent["id"], "Run a turn", "delete-inflight-start")
        self.runtime.dispatch(agent["id"])
        fixture.fixture.eventually(lambda: self.runtime.agent(agent["id"]).get("turnId"))
        self.assertEqual(len(self.slots()), baseline + 1)
        return self.runtime.agent(agent["id"]), baseline

    def assert_deleted_and_released(self, agent_id, baseline):
        current = self.runtime.agent(agent_id)
        self.assertTrue(current.get("deletedAt"))
        self.assertFalse(current["autoWake"])
        self.assertEqual(current["status"], "paused")
        self.assertFalse(current.get("inFlight"))
        self.assertIsNone(current.get("turnId"))
        self.assertEqual(len(self.slots()), baseline)

    def test_interrupt_answered_before_delete_returns_releases_slot(self):
        agent, baseline = self.running_chat()
        deleted = self.runtime.delete_conversation(agent["id"])
        self.assertEqual(deleted["deleted"], [agent["id"]])
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_interrupt_timeout_releases_slot_and_late_completion_stays_tombstoned(self):
        agent, baseline = self.running_chat()
        server = self.runtime.server
        interrupt_calls = []
        original = server.call

        def call(method, params, timeout=60):
            if method == "turn/interrupt":
                interrupt_calls.append((method, params))
                raise RuntimeError("turn/interrupt response timed out; outcome unknown")
            return original(method, params, timeout)

        with patch.object(server, "call", side_effect=call):
            self.runtime.delete_conversation(agent["id"])
            self.assert_deleted_and_released(agent["id"], baseline)
            self.assertIn("outcome unknown", self.runtime.agent(agent["id"]).get("error", ""))
            repeated = self.runtime.delete_conversation(agent["id"])
        self.assertEqual(len(interrupt_calls), 1)
        self.assertEqual(repeated["deleted"], [agent["id"]])
        self.assert_deleted_and_released(agent["id"], baseline)

        # The native notification router must ignore late work for a tombstone.
        server.active_turns.pop(agent["threadId"], None)
        server.complete(agent["threadId"], agent["turnId"], "Late result")
        self.assert_deleted_and_released(agent["id"], baseline)

    def test_idle_delete_and_repeated_delete_remain_safe(self):
        agent = self.runtime.create(
            {"name": "Idle delete", "cwd": self.temp.name, "prompt": "Unused task"}, defer=True
        )
        baseline = len(self.slots())
        self.assertEqual(self.runtime.delete_conversation(agent["id"])["deleted"], [agent["id"]])
        self.assertEqual(self.runtime.delete_conversation(agent["id"])["deleted"], [agent["id"]])
        self.assert_deleted_and_released(agent["id"], baseline)


if __name__ == "__main__":
    unittest.main()
