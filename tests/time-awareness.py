#!/usr/bin/env python3
"""Clock metadata stays stable across delivery, queue changes, retries, and restart."""

import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "workspace_fixture", Path(__file__).with_name("workspace-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_time import append_message_clocks, message_clock, stamp_tool_result


class TimeAwareness(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    start = f.WorkspaceContract.start
    events = f.WorkspaceContract.events

    def metadata(self, key):
        with self.runtime.db() as db:
            return json.loads(
                db.execute(
                    "SELECT record FROM runtime_event_meta WHERE id=?", (key,)
                ).fetchone()[0]
            )

    def tool(self, agent, name="orchestration_status", key="clock-tool"):
        self.runtime.dynamic(
            {
                "id": key,
                "params": {
                    "threadId": agent["threadId"],
                    "callId": key,
                    "tool": name,
                    "arguments": {},
                },
            }
        )
        return self.runtime.server.responses[-1]["result"]

    def test_queue_retry_and_priority_preserve_acceptance_time_and_ui_text(self):
        agent = self.lead()
        self.runtime.send(agent["id"], "First question", "one")
        self.runtime.send(agent["id"], "Second question", "two")
        original = self.metadata("two")
        self.runtime.send(agent["id"], "Second question", "two")
        self.runtime.queue_action(agent["id"], {"action": "first", "message_id": "two"})
        self.assertEqual(original, self.metadata("two"))
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(agent["id"]).get("turnId"))
        params = [p for m, p in self.runtime.server.calls if m == "turn/start"][-1]
        text = params["input"][0]["text"]
        self.assertIn(
            message_clock("two", original["acceptedAt"])["accepted_at_utc"], text
        )
        with self.runtime.db() as db:
            items = self.runtime.records(db, "items")
        self.assertTrue(items)
        self.assertNotIn("Time awareness", json.dumps(items))
        self.assertEqual(
            {r["text"] for r in self.events(agent)},
            {"First question", "Second question"},
        )

    def test_legacy_queue_does_not_invent_receipt_time_from_priority(self):
        agent = self.lead()
        self.runtime.send(agent["id"], "Legacy question", "legacy")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("DELETE FROM runtime_event_meta WHERE id='legacy'")
            db.execute("UPDATE runtime_events SET created=1 WHERE id='legacy'")
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(agent["id"]).get("turnId"))
        params = [p for m, p in self.runtime.server.calls if m == "turn/start"][-1]
        self.assertEqual(params["input"][0]["text"], "Legacy question")

    def test_edit_updates_acceptance_once_without_reorder_clock(self):
        agent = self.lead()
        self.runtime.send(agent["id"], "Before", "edit")
        before = self.metadata("edit")
        with patch("codex_work.time.time", return_value=before["acceptedAt"] + 12):
            self.runtime.queue_action(
                agent["id"], {"action": "edit", "message_id": "edit", "text": "After"}
            )
        after = self.metadata("edit")
        self.assertEqual(after["acceptedAt"], before["acceptedAt"] + 12)
        self.runtime.queue_action(
            agent["id"], {"action": "edit", "message_id": "edit", "text": "After"}
        )
        self.assertEqual(after, self.metadata("edit"))

    def test_steer_timestamp_is_durable_and_ui_is_clean(self):
        agent = self.start(self.lead())
        self.runtime.send(agent["id"], "Correct this", "steer", delivery="steer")
        wire = [p for m, p in self.runtime.server.calls if m == "turn/steer"][-1]
        self.assertIn(
            message_clock("steer", self.metadata("steer")["acceptedAt"])[
                "accepted_at_utc"
            ],
            wire["input"][0]["text"],
        )
        self.runtime.send(agent["id"], "Correct this", "steer", delivery="steer")
        self.assertEqual(
            1, len([1 for m, p in self.runtime.server.calls if m == "turn/steer"])
        )
        with self.runtime.db() as db:
            item = json.loads(
                db.execute(
                    "SELECT record FROM runtime_items WHERE id=?",
                    (agent["id"] + ":steer",),
                ).fetchone()[0]
            )
        self.assertEqual(item["text"], "Correct this")

    def test_success_and_error_results_replay_byte_identically_after_restart(self):
        agent = self.runtime.prepare(self.lead())
        success = self.tool(agent)
        error = self.tool(agent, "does_not_exist", "clock-error")
        self.assertTrue(success["success"])
        self.assertFalse(error["success"])
        self.assertEqual(len(success["contentItems"]), 2)
        json.loads(success["contentItems"][0]["text"])
        self.assertIn("finalized at", error["contentItems"][-1]["text"])
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.assertEqual(success, self.tool(agent))
        self.assertEqual(error, self.tool(agent, "does_not_exist", "clock-error"))

    def test_stamp_appends_without_changing_input_prefix_or_tool_body(self):
        original = "User text\n" * 2000
        stamped = append_message_clocks(original, [message_clock("a", 1788652800)])
        self.assertTrue(stamped.startswith(original))
        result = {
            "success": True,
            "contentItems": [{"type": "inputText", "text": '{"value":1}'}],
        }
        stamped_result = stamp_tool_result(result, 1788652800)
        self.assertEqual(stamped_result["contentItems"][:1], result["contentItems"])
        self.assertEqual(1, len(result["contentItems"]))
        self.assertEqual(stamped_result, stamp_tool_result(result, 1788652800))


if __name__ == "__main__":
    unittest.main()
