#!/usr/bin/env python3
"""Queue ordering, revision conflicts, and durable mutation receipts."""
import base64
import importlib.util
import json
from pathlib import Path
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "queue_workspace_fixture", Path(__file__).with_name("workspace-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class QueueOrderContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead

    def queue(self, key):
        return self.runtime.queue_action(key)

    def seed(self, count=3):
        key = self.lead()["id"]
        for index in range(count):
            self.runtime.send(key, f"Message {index}", f"message-{index}")
        return key

    def request(self, key, action="reorder", **changes):
        current = self.queue(key)
        return {"action": action, "request_id": f"request-{action}",
                "expected_revision": current["revision"], **changes}

    def metadata(self, key):
        with self.runtime.db() as db:
            return json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (key,)).fetchone()[0])

    def order(self, key):
        return [row["id"] for row in self.queue(key)["items"]]

    def test_reorder_survives_restart_and_controls_real_dispatch(self):
        key = self.seed()
        clocks = {item: self.metadata(item) for item in self.order(key)}
        request = self.request(key, ordered_ids=["message-2", "message-0", "message-1"])
        receipt = self.runtime.queue_action(key, request)
        self.assertTrue(receipt["capabilities"]["reorder"])
        self.assertEqual(self.order(key), request["ordered_ids"])
        self.assertEqual(clocks, {item: self.metadata(item) for item in self.order(key)})
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.assertEqual(self.runtime.queue_action(key, request), receipt)
        self.assertEqual(self.queue(key)["revision"], receipt["revision"])
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(key).get("turnId"))
        text = next(params for method, params in self.runtime.server.calls if method == "turn/start")["input"][0]["text"]
        self.assertTrue(text.startswith("Message 2\n\nMessage 0\n\nMessage 1"), text)
        self.assertEqual(self.queue(key)["items"], [])
        self.assertEqual(self.runtime.queue_action(key, request), receipt)

    def test_lost_reorder_response_retry_preserves_later_order(self):
        key = self.seed()
        first = self.request(key, ordered_ids=["message-2", "message-0", "message-1"])
        receipt = self.runtime.queue_action(key, first)
        second = self.request(key, request_id="second", ordered_ids=["message-1", "message-2", "message-0"])
        current = self.runtime.queue_action(key, second)
        self.assertEqual(self.runtime.queue_action(key, first), receipt)
        self.assertEqual(self.queue(key)["revision"], current["revision"])
        self.assertEqual(self.order(key), second["ordered_ids"])
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.queue_action(key, {**first, "ordered_ids": second["ordered_ids"]})

    def test_edit_and_cancel_retry_preserve_later_changes(self):
        key = self.seed()
        edit = self.request(key, "edit", message_id="message-0", text="First edit", expectedText="Message 0")
        with patch("codex_work.time.time", return_value=2000):
            receipt = self.runtime.queue_action(key, edit)
        later = self.request(key, "edit", request_id="later-edit", message_id="message-0", text="Later edit", expectedText="First edit")
        with patch("codex_work.time.time", return_value=3000):
            self.runtime.queue_action(key, later)
        self.assertEqual(self.runtime.queue_action(key, edit), receipt)
        self.assertEqual(self.queue(key)["items"][0]["text"], "Later edit")
        self.assertEqual(self.metadata("message-0")["acceptedAt"], 3000)
        cancel = self.request(key, "cancel", message_id="message-0")
        cancelled = self.runtime.queue_action(key, cancel)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.assertEqual(self.runtime.queue_action(key, cancel), cancelled)
        self.assertNotIn("message-0", self.order(key))
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.queue_action(key, {**cancel, "message_id": "message-1"})

    def test_stale_revision_catches_enqueue_edit_and_order_aba(self):
        key = self.seed()
        stale = self.request(key, ordered_ids=self.order(key))
        self.runtime.queue_action(key, self.request(key, "edit", message_id="message-0", text="Changed"))
        self.runtime.queue_action(key, self.request(key, "edit", request_id="restore-text", message_id="message-0", text="Message 0"))
        with self.assertRaisesRegex(ValueError, "queue changed"):
            self.runtime.queue_action(key, stale)
        stale = self.request(key, ordered_ids=self.order(key))
        self.runtime.queue_action(key, self.request(key, request_id="reverse", ordered_ids=list(reversed(self.order(key)))))
        self.runtime.queue_action(key, self.request(key, request_id="restore-order", ordered_ids=stale["ordered_ids"]))
        with self.assertRaisesRegex(ValueError, "queue changed"):
            self.runtime.queue_action(key, stale)
        stale = self.request(key, ordered_ids=self.order(key))
        self.runtime.send(key, "New message", "new")
        with self.assertRaisesRegex(ValueError, "queue changed"):
            self.runtime.queue_action(key, stale)

    def test_foreign_missing_duplicate_and_invalid_ids_fail_atomically(self):
        key = self.seed()
        other = self.lead("Other")["id"]
        self.runtime.send(other, "Other", "foreign")
        before = self.queue(key)
        for ordered in (["message-0", "message-1"], ["message-0", "message-1", "foreign"],
                        ["message-0", "message-1", "missing"], ["message-0", "message-1", "message-1"],
                        ["message-0", "message-1", {}], "message-0", None):
            with self.subTest(ordered=ordered), self.assertRaisesRegex(ValueError, "exactly once"):
                self.runtime.queue_action(key, self.request(key, ordered_ids=ordered))
            self.assertEqual(self.queue(key), before)
        for changes in ({"request_id": None}, {"request_id": ""}, {"expected_revision": None}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.runtime.queue_action(key, {**self.request(key, ordered_ids=self.order(key)), **changes})
        self.assertEqual(self.order(other), ["foreign"])

    def test_old_epoch_and_reserved_messages_are_never_editable(self):
        key = self.seed()
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET epoch=epoch-1 WHERE id='message-0'")
            db.execute("UPDATE runtime_events SET status='dispatching' WHERE id='message-1'")
        self.assertEqual(self.order(key), ["message-2"])
        for target in ["message-0", "message-1"]:
            with self.assertRaisesRegex(ValueError, "already left"):
                self.runtime.queue_action(key, {"action": "cancel", "message_id": target})
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='message-0'").fetchone()[0], "pending")
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='message-1'").fetchone()[0], "dispatching")

    def test_dispatch_wins_race_without_edit_or_cancel_of_reserved_input(self):
        key = self.seed()
        request = self.request(key, "edit", message_id="message-0", text="Too late")
        entered = threading.Event()
        submitted = []
        errors = []
        def edit():
            entered.set()
            try:
                self.runtime.queue_action(key, request)
            except ValueError as error:
                errors.append(str(error))
        with self.runtime.lock, patch.object(self.runtime.pool, "submit", side_effect=lambda *args: submitted.append(args)):
            worker = threading.Thread(target=edit)
            worker.start()
            self.assertTrue(entered.wait(1))
            self.runtime.dispatch()
        worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertIn("queue changed", errors[0])
        self.assertEqual([row["text"] for row in submitted[0][2]], ["Message 0", "Message 1", "Message 2"])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT text,status FROM runtime_events WHERE id='message-0'").fetchone()[:], ("Message 0", "reserved"))

    def test_assets_and_delivery_metadata_survive_reorder_and_text_removal(self):
        key = self.seed(1)
        asset = self.runtime.upload_asset({"agent": key, "name": "report.md", "base64": base64.b64encode(b"# Report").decode()})
        self.runtime.send(key, "Report", "attachment", assets=[asset["id"]], delivery="after_tool")
        before = self.metadata("attachment")
        self.runtime.queue_action(key, self.request(key, ordered_ids=["attachment", "message-0"]))
        self.assertEqual(self.metadata("attachment"), before)
        row = self.queue(key)["items"][0]
        self.assertEqual(row["delivery"], "queue")
        self.assertEqual(row["requestedDelivery"], "after_tool")
        self.assertEqual(row["assets"][0]["id"], asset["id"])
        self.assertNotIn("path", row["assets"][0])
        self.runtime.queue_action(key, self.request(key, "edit", message_id="attachment", text=""))
        self.assertEqual(self.queue(key)["items"][0]["text"], "")
        self.assertEqual(self.metadata("attachment")["assets"], [asset["id"]])
        with self.assertRaisesRegex(ValueError, "Supply a message"):
            self.runtime.queue_action(key, self.request(key, "edit", request_id="empty-no-assets", message_id="message-0", text=""))
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(key).get("turnId"))
        params = next(params for method, params in self.runtime.server.calls if method == "turn/start")
        self.assertIn("report.md", params["input"][1]["text"])

    def test_equal_timestamps_become_deterministic_without_changing_system_slot(self):
        key = self.seed()
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(key, db)
            self.runtime.enqueue(db, agent, "work_update", "System input", "system")
            db.execute("UPDATE runtime_events SET created=100 WHERE id IN ('message-0','message-1')")
            db.execute("UPDATE runtime_events SET created=150 WHERE id='system'")
            db.execute("UPDATE runtime_events SET created=200 WHERE id='message-2'")
        self.runtime.queue_action(key, self.request(key, ordered_ids=["message-2", "message-1", "message-0"]))
        with self.runtime.db() as db:
            all_events = db.execute("SELECT id,created FROM runtime_events WHERE agent=? ORDER BY created", (key,)).fetchall()
        self.assertEqual([r["id"] for r in all_events], ["message-2", "message-1", "system", "message-0"])
        self.assertEqual(all_events[2]["created"], 150)
        self.assertLess(all_events[0]["created"], all_events[1]["created"])
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(key).get("turnId"))
        text = next(params for method, params in self.runtime.server.calls if method == "turn/start")["input"][0]["text"]
        self.assertLess(text.index("Message 2"), text.index("Message 1"))
        self.assertLess(text.index("Message 1"), text.index("System input"))
        self.assertLess(text.index("System input"), text.index("Message 0"))

    def test_equal_timestamp_slots_stay_before_system_event(self):
        key = self.seed(2)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enqueue(db, self.runtime.agent(key, db), "work_update", "System", "system")
            db.execute("UPDATE runtime_events SET created=100 WHERE agent=?", (key,))
            db.execute("DELETE FROM runtime_event_meta WHERE id='message-0'")
        self.assertNotIn("acceptedAt", self.queue(key)["items"][0])
        self.runtime.queue_action(key, self.request(key, ordered_ids=["message-1", "message-0"]))
        with self.runtime.db() as db:
            rows = db.execute("SELECT id,created FROM runtime_events WHERE agent=? ORDER BY created", (key,)).fetchall()
        self.assertEqual([row["id"] for row in rows], ["message-1", "message-0", "system"])
        self.assertLess(rows[0]["created"], rows[1]["created"])
        self.assertLess(rows[1]["created"], 100)
        self.assertEqual(rows[2]["created"], 100)
        self.assertNotIn("acceptedAt", self.queue(key)["items"][1])

    def test_no_timestamp_space_rejects_without_moving_system_events(self):
        key = self.seed(2)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(key, db)
            self.runtime.enqueue(db, agent, "work_update", "Before", "before")
            self.runtime.enqueue(db, agent, "work_update", "After", "after")
            db.execute("UPDATE runtime_events SET rowid=-1 WHERE id='before'")
            db.execute("UPDATE runtime_events SET created=100 WHERE agent=?", (key,))
        before = self.queue(key)
        with self.assertRaisesRegex(ValueError, "cannot be reordered safely"):
            self.runtime.queue_action(key, self.request(key, ordered_ids=["message-1", "message-0"]))
        self.assertEqual(self.queue(key), before)
        with self.runtime.db() as db:
            self.assertEqual([row[0] for row in db.execute("SELECT id FROM runtime_events WHERE agent=? ORDER BY created,rowid", (key,))],
                             ["before", "message-0", "message-1", "after"])

    def test_failed_receipt_write_rolls_back_order_and_revision(self):
        key = self.seed()
        before = self.queue(key)
        request = self.request(key, ordered_ids=list(reversed(self.order(key))))
        with patch.object(self.runtime, "save_receipt", side_effect=RuntimeError("disk write failed")):
            with self.assertRaisesRegex(RuntimeError, "disk write failed"):
                self.runtime.queue_action(key, request)
        self.assertEqual(self.queue(key), before)
        self.runtime.queue_action(key, request)
        self.assertEqual(self.order(key), request["ordered_ids"])


if __name__ == "__main__":
    unittest.main()
