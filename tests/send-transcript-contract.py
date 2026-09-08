#!/usr/bin/env python3
"""Receipt visibility across queue reservation and native input materialization."""

import base64
import importlib.util
import json
from contextlib import contextmanager
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "workspace_fixture", Path(__file__).with_name("workspace-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class SendTranscriptContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    start = f.WorkspaceContract.start

    def messages(self, agent):
        result = []
        for item in self.runtime.transcript(agent)["items"]:
            if item.get("inputs"):
                result.extend(r for r in item["inputs"] if r["kind"] == "user")
            elif item["role"] == "user":
                result.append(item)
        return result

    def tool_fixture(self, result, *, alias=False, native=None, account="default"):
        a = self.start(self.lead())
        payload = {"id": "receipt-tool", "type": "dynamicToolCall", "tool": "orchestration_send",
                   "arguments": {"agent_id": "worker", "text": "Review"},
                   "status": "inProgress", "success": None, "contentItems": None}
        payload.update(native or {})
        prefix = (account + ":" if account != "default" else "") + a["threadId"] + ":"
        request_id = prefix + ("spawn:explicit-request" if alias else payload["id"])
        with self.runtime.db() as db:
            a.update(accountKey=account, status="failed", inFlight=False, turnId=None)
            self.runtime.put(db, "agents", a)
            self.runtime.item(db, a["id"], payload["id"], "output", json.dumps(payload),
                              "dynamicToolCall", toolStatus="running", turnId="old-turn")
            if result is not None:
                db.execute("INSERT INTO runtime_tool_results VALUES (?,?)", (request_id, json.dumps(result)))
            if alias:
                db.execute("INSERT INTO runtime_tool_request_aliases VALUES (?,?,?)",
                           (a["id"], payload["id"], request_id))
        return a, request_id

    def test_durable_tool_result_recovers_missing_native_completion(self):
        for success, account, alias in ((False, "default", False), (True, "profile-fixture", True)):
            with self.subTest(success=success, account=account):
                result = {"success": success, "contentItems": [{"type": "inputText",
                          "text": 'no active turn to steer' if not success else 'Recorded result'}]}
                a, _ = self.tool_fixture(result, account=account, alias=alias)
                item = self.runtime.transcript(a["id"])["items"][-1]
                self.assertEqual(item["toolStatus"], "completed" if success else "failed")
                payload = json.loads(item["text"])
                self.assertEqual(payload["contentItems"], result["contentItems"])
                self.assertEqual(payload["success"], success)
                self.assertEqual(payload["arguments"]["text"], "Review")
                with self.runtime.db() as db:
                    stored = json.loads(db.execute("SELECT record FROM runtime_items WHERE id=?", (item["id"],)).fetchone()[0])
                self.assertEqual(stored["toolStatus"], "running", "Read must not rewrite historical evidence")

    def test_empty_or_malformed_native_content_keeps_history_available(self):
        from codex_sync import SyncStore
        for contents in (None, [], {}, "unexpected", [None, 7, {}, {"type": "inputText", "text": None}]):
            with self.subTest(contents=contents):
                a, _ = self.tool_fixture(None, native={"contentItems": contents})
                sync = SyncStore(self.runtime.db, self.runtime.snapshot, self.runtime.transcript)
                result = sync.pull("transcript:" + a["id"])
                transcript = json.loads(result["documents"][0]["payload"])
                item = transcript["items"][-1]
                self.assertEqual(item["toolStatus"], "interrupted")
                self.assertEqual(json.loads(item["text"])["contentItems"], contents)

    def test_missing_or_ambiguous_tool_receipt_does_not_invent_completion(self):
        a, _ = self.tool_fixture(None)
        self.assertEqual(self.runtime.transcript(a["id"])["items"][-1]["toolStatus"], "interrupted")
        a, receipt = self.tool_fixture({"success": True, "contentItems": []}, alias=True)
        with self.runtime.db() as db:
            db.execute("INSERT INTO runtime_tool_request_aliases VALUES (?,?,?)", (a["id"], "receipt-tool", receipt + ":ambiguous"))
        self.assertEqual(self.runtime.transcript(a["id"])["items"][-1]["toolStatus"], "interrupted")

    def test_large_receipt_keeps_transcript_bounded_and_valid_json(self):
        a, _ = self.tool_fixture({"success": True, "contentItems": [{"type": "inputText", "text": "x" * 100000}]})
        item = self.runtime.transcript(a["id"])["items"][-1]
        self.assertEqual(item["toolStatus"], "completed")
        self.assertTrue(item["truncated"])
        self.assertLess(len(item["text"]), 14000)
        self.assertIn("Result truncated", json.loads(item["text"])["contentItems"][0]["text"])

    def test_native_completion_and_other_agents_receipts_are_not_overridden(self):
        a, _ = self.tool_fixture({"success": True, "contentItems": []}, native={"status": "failed", "success": False})
        item = self.runtime.transcript(a["id"])["items"][-1]
        self.assertFalse(json.loads(item["text"])["success"])
        a, receipt = self.tool_fixture(None)
        with self.runtime.db() as db:
            db.execute("INSERT INTO runtime_tool_results VALUES (?,?)", (receipt + ":other", json.dumps({"success": True})))
            db.execute("INSERT INTO runtime_tool_request_aliases VALUES (?,?,?)", ("other-agent", "receipt-tool", receipt + ":other"))
        self.assertEqual(self.runtime.transcript(a["id"])["items"][-1]["toolStatus"], "interrupted")

    def test_actual_dispatch_keeps_each_receipt_and_attachment_visible(self):
        key = self.lead()["id"]
        asset = self.runtime.upload_asset({"agent": key, "name": "note.txt",
                                          "base64": base64.b64encode(b"attached").decode()})
        self.runtime.send(key, "First input", "one")
        self.runtime.send(key, "", "two", assets=[asset["id"]])
        original = self.messages(key)
        self.assertEqual([r["clientMessageId"] for r in original], ["one", "two"])
        self.assertEqual(original[1]["assets"][0]["id"], asset["id"])
        self.assertTrue(all(r["materialized"] is False for r in original))
        self.assertNotIn("path", original[1]["assets"][0])
        work = []
        with patch.object(self.runtime.pool, "submit", side_effect=lambda *args: work.append(args)):
            self.runtime.dispatch()
        self.assertEqual(len(work), 1)
        reserved = self.messages(key)
        self.assertEqual([r["deliveryStatus"] for r in reserved], ["reserved", "reserved"])
        self.assertEqual([r["clientMessageId"] for r in reserved], ["one", "two"])
        # Exercise the committed dispatch state before materialization, then
        # run the real start path and native acknowledgement.
        with self.runtime.db() as db:
            for event_id in ("one", "two"):
                db.execute("UPDATE runtime_events SET status='dispatching' WHERE id=?", (event_id,))
        dispatching = self.messages(key)
        self.assertEqual([r["deliveryStatus"] for r in dispatching], ["dispatching", "dispatching"])
        fn, *args = work[0]
        fn(*args)
        delivered = self.messages(key)
        self.assertEqual(len(delivered), 2, "Materialization must not duplicate the second batch input")
        self.assertEqual([r["clientMessageId"] for r in delivered], ["one", "two"])
        self.assertEqual([r["id"] for r in delivered], ["one", "two"])
        self.assertEqual([r["deliveryStatus"] for r in delivered], ["delivered", "delivered"])
        self.assertTrue(all(r["materialized"] is True for r in delivered))
        with self.runtime.db() as db:
            pointers = [json.loads(r[0])["transcriptItemId"] for r in db.execute(
                "SELECT record FROM runtime_event_meta WHERE id IN ('one','two') ORDER BY id")]
        self.assertEqual(pointers, [key + ":one", key + ":one"])
        self.assertEqual([r["at"] for r in delivered], [r["at"] for r in original])
        self.assertEqual(delivered[1]["assets"][0]["id"], asset["id"])

    def test_edit_cancel_failure_and_uncertainty_keep_the_correct_content(self):
        key = self.lead()["id"]
        self.runtime.send(key, "Original", "one")
        self.runtime.queue_action(key, {"action": "edit", "message_id": "one", "text": "Edited"})
        self.assertEqual(self.messages(key)[0]["text"], "Edited")
        for status in ("reserved", "dispatching", "uncertain", "failed"):
            with self.runtime.db() as db:
                db.execute("UPDATE runtime_events SET status=?,error=? WHERE id='one'", (status, "fixture error"))
            visible = self.messages(key)
            self.assertEqual(len(visible), 1)
            self.assertEqual(visible[0]["clientMessageId"], "one")
            self.assertEqual(visible[0]["text"], "Edited")
            self.assertEqual(visible[0]["deliveryStatus"], status)
            self.assertEqual(visible[0]["deliveryError"], "fixture error")
            self.assertFalse(visible[0]["pending"])
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='pending' WHERE id='one'")
        self.runtime.queue_action(key, {"action": "cancel", "message_id": "one"})
        self.assertEqual(self.messages(key), [])

    def test_failure_before_native_input_preserves_receipt(self):
        key = self.lead()["id"]
        self.runtime.send(key, "Keep this instruction", "one")
        with patch.object(self.runtime, "prepare", side_effect=ValueError("Fixture preparation failure")):
            self.runtime.dispatch()
            f.eventually(lambda: self.runtime.agent(key)["status"] == "failed")
        visible = self.messages(key)
        self.assertEqual(len(visible), 1)
        self.assertEqual(visible[0]["clientMessageId"], "one")
        self.assertEqual(visible[0]["deliveryStatus"], "failed")
        self.assertEqual(visible[0]["text"], "Keep this instruction")

    def test_steer_failure_is_visible_with_its_exact_receipt(self):
        agent = self.start(self.lead())
        self.runtime.server.steer_error = RuntimeError("Fixture steer rejection")
        with self.assertRaisesRegex(RuntimeError, "Fixture steer rejection"):
            self.runtime.send(agent["id"], "Second instruction", "steer-one", delivery="steer")
        visible = [r for r in self.messages(agent["id"]) if r.get("clientMessageId") == "steer-one"]
        self.assertEqual(len(visible), 1)
        self.assertEqual(visible[0]["deliveryStatus"], "failed")
        self.assertEqual(visible[0]["text"], "Second instruction")

    def test_pending_receipts_do_not_scan_older_transcript_payloads(self):
        key = self.lead()["id"]
        self.runtime.send(key, "Keep this instruction", "one")

        def add_history(first, count):
            with self.runtime.db() as db:
                db.executemany("INSERT INTO runtime_items VALUES (?,?,?,?)", (
                    (key + ":history-" + str(n), key, json.dumps({
                        "id": key + ":history-" + str(n), "role": "tool", "text": "x" * 4000,
                        "at": -n}), -n)
                    for n in range(first, first + count)))

        real_db = self.runtime.db

        def measure():
            work = [0]

            @contextmanager
            def measured_db():
                with real_db() as db:
                    def step():
                        work[0] += 1
                        return 0
                    db.set_progress_handler(step, 100)
                    yield db
            with patch.object(self.runtime, "db", measured_db):
                visible = self.messages(key)
            self.assertEqual([r["clientMessageId"] for r in visible], ["one"])
            return work[0]

        add_history(1, 130)
        before = measure()
        add_history(131, 2000)
        after = measure()
        self.assertLessEqual(after, before + 10,
                             "Outstanding receipts must use indexed identities, not scan history")

    def test_legacy_input_without_receipt_fields_remains_supported(self):
        key = self.lead()["id"]
        with self.runtime.db() as db:
            self.runtime.item(db, key, "legacy", "user", "Imported",
                              inputs=[{"kind": "user", "text": "Imported", "assets": []}])
        self.assertEqual(self.messages(key)[0]["text"], "Imported")


if __name__ == "__main__":
    unittest.main()
