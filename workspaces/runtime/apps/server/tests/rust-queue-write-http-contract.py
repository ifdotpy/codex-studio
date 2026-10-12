#!/usr/bin/env python3
"""Queue writes through the HTTP harness against Python and Rust backends."""

from __future__ import annotations

import importlib.util
import json
import sqlite3
import sys
import unittest
from contextlib import closing
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "src"))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend

fixture_spec = importlib.util.spec_from_file_location("queue_write_fixture", HERE / "runtime-contract.py")
fixture = importlib.util.module_from_spec(fixture_spec)
assert fixture_spec.loader is not None
fixture_spec.loader.exec_module(fixture)


def seed_queue(backend: object) -> str:
    backend.stop()

    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, fixture.FakeServer)
    try:
        agent = runtime.create({
            "name": "Queue write contract",
            "cwd": str(backend.cwd),
            "prompt": "Seed Python-written queue events for the HTTP contract.",
            "autoWake": True,
        }, defer=True)
        agent.update(autoWake=True, status="idle")
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
            for index in range(3):
                runtime.enqueue(database, agent, "user", f"Message {index}", f"queue-{index}")
            agent.update(autoWake=False, status="idle")
            runtime.put(database, "agents", agent)
        return agent["id"]
    finally:
        runtime.close()


def queue_state(state: Path):
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        tables = [row[0] for row in database.execute(
            "SELECT name FROM sqlite_master WHERE type='table' "
            "AND (name LIKE 'runtime_%' OR name LIKE 'sync_%') ORDER BY name"
        )]
        return {
            table: sorted(database.execute(f'SELECT * FROM "{table}"').fetchall(), key=repr)
            for table in tables
        }


class QueueWriteHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_queue(cls.backend)
        cls.backend.start()

    def headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def request(self, body):
        return self.backend.request("POST", "/api/queue", body=body, headers=self.headers())

    def get_queue(self):
        return self.backend.request("GET", f"/api/queue?agent={self.agent_id}", headers=self.headers())

    def test_reads_python_written_queue_and_applies_durable_mutations(self):
        initial = self.get_queue()
        self.assertEqual(initial.status, 200, initial.body[:1000])
        self.assertEqual([item["id"] for item in initial.json()["items"]], ["queue-0", "queue-1", "queue-2"])
        self.assertEqual(initial.json()["items"][0]["text"], "Message 0")

        edit = {
            "agent": self.agent_id, "action": "edit", "request_id": "edit-1",
            "message_id": "queue-0", "expected_revision": initial.json()["revision"],
            "expectedText": "Message 0", "text": "Edited message",
        }
        edited = self.request(edit)
        self.assertEqual(edited.status, 200, edited.body[:1000])
        self.assertEqual(edited.json()["status"], "updated")
        self.assertEqual(self.request(edit).json(), edited.json())
        self.assertEqual(self.get_queue().json()["items"][0]["text"], "Edited message")

        current = self.get_queue().json()
        reorder = {
            "agent": self.agent_id, "action": "reorder", "request_id": "reorder-1",
            "expected_revision": current["revision"],
            "ordered_ids": ["queue-2", "queue-1", "queue-0"],
        }
        response = self.request(reorder)
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual([item["id"] for item in self.get_queue().json()["items"]], reorder["ordered_ids"])

        current = self.get_queue().json()
        cancel = {
            "agent": self.agent_id, "action": "cancel", "request_id": "cancel-1",
            "expected_revision": current["revision"], "message_id": "queue-1",
        }
        self.assertEqual(self.request(cancel).status, 200)
        self.assertNotIn("queue-1", [item["id"] for item in self.get_queue().json()["items"]])

    def test_stale_and_invalid_mutations_leave_state_unchanged(self):
        initial = self.get_queue().json()
        before = queue_state(self.backend.state)
        stale = self.request({
            "agent": self.agent_id, "action": "reorder", "request_id": "stale-1",
            "expected_revision": "stale-revision", "ordered_ids": ["queue-2", "queue-1", "queue-0"],
        })
        self.assertEqual(stale.status, 400, stale.body[:1000])
        self.assertEqual(queue_state(self.backend.state), before)
        invalid = self.request({
            "agent": self.agent_id, "action": "send_now", "message_id": "queue-0",
        })
        self.assertEqual(invalid.status, 400, invalid.body[:1000])
        self.assertEqual(queue_state(self.backend.state), before)
        blank_identity = self.request({
            "agent": self.agent_id, "action": "send_now", "request_id": "  ",
            "message_id": "queue-0",
        })
        self.assertEqual(blank_identity.status, 400, blank_identity.body[:1000])
        self.assertEqual(queue_state(self.backend.state), before)
        self.assertEqual(self.get_queue().json(), initial)

    def test_z_first_and_send_now_move_the_requested_message(self):
        current = self.get_queue().json()
        before_ids = [item["id"] for item in current["items"]]
        first_id = before_ids[-1]
        first = self.request({
            "agent": self.agent_id, "action": "first", "request_id": "first-1",
            "expected_revision": current["revision"], "message_id": first_id,
        })
        self.assertEqual(first.status, 200, first.body[:1000])
        after_first = self.get_queue().json()
        self.assertEqual([item["id"] for item in after_first["items"]], [first_id, *before_ids[:-1]])

        current = after_first
        send_now_id = current["items"][-1]["id"]
        send_now_body = {
            "agent": self.agent_id, "action": "send_now", "request_id": "send-now-1",
            "expected_revision": current["revision"], "message_id": send_now_id,
        }
        send_now = self.request(send_now_body)
        self.assertEqual(send_now.status, 200, send_now.body[:1000])
        replay = self.request(send_now_body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["status"], send_now.json()["status"])
        self.assertEqual(replay.json()["revision"], send_now.json()["revision"])
        self.assertEqual(replay.json()["capabilities"], send_now.json()["capabilities"])
        queued = self.get_queue().json()["items"]
        self.assertEqual(queued[0]["id"], send_now_id)
        self.assertEqual(queued[0]["delivery"], "steer")


if __name__ == "__main__":
    unittest.main()
