#!/usr/bin/env python3
"""Broadcast completion boundaries. Isolated state and fake native protocol."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import importlib.util
import json
from pathlib import Path
import sys
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class BroadcastLifecycle(unittest.TestCase):
    setUp = fixture.RuntimeContract.setUp
    tearDown = fixture.RuntimeContract.tearDown
    lead = fixture.RuntimeContract.lead

    def worker(self, lead):
        child = self.runtime.create({"name": "Review", "prompt": "Finish the assigned review", "role": "reviewer"}, lead["id"])
        fixture.eventually(lambda: self.runtime.agent(child["id"])["status"] == "running")
        return self.runtime.agent(child["id"])

    def event(self, agent, key):
        with self.runtime.db() as db:
            row = db.execute("SELECT * FROM runtime_events WHERE id=?", (f"chat:{key}:{agent['id']}",)).fetchone()
            return dict(row) if row else None

    def finish(self, agent):
        self.runtime.server.complete(agent["threadId"], agent["turnId"])

    def test_broadcast_received_before_completion_cannot_start_another_turn(self):
        lead = self.lead()
        child = self.worker(lead)
        import codex_agent_management

        archive_started = threading.Event()
        archive_release = threading.Event()
        archive_finished = threading.Event()
        archive = codex_agent_management._archive_reviewer

        def gated_archive(*args):
            archive_started.set()
            if not archive_release.wait(5):
                raise AssertionError("Test did not release reviewer archival")
            try:
                return archive(*args)
            finally:
                archive_finished.set()

        with patch.object(codex_agent_management, "_archive_reviewer", gated_archive):
            with self.runtime.lock:
                first = self.runtime.chat_message(lead["id"], "broadcast", "Policy update only", "before-finish")
                self.assertEqual(first["deliveries"][child["id"]], "queued")
                self.finish(child)
                before_parent_receipt = self.runtime.agent(child["id"])
                self.assertEqual(before_parent_receipt["status"], "completed")
                self.assertEqual(before_parent_receipt["lastCompletedTurnStatus"], "completed")
                event = self.event(child, "before-finish")
                self.assertEqual(event["status"], "stored_only")
                self.assertIn("Assignment completed", event["error"])
                history = self.runtime.chat_read(first["room"], child["id"])
                self.assertEqual(history["messages"][-1]["deliveries"][child["id"]], "stored_only")
                # A duplicate completion or admission receipt cannot restore delivery.
                self.finish(child)
                self.assertEqual(self.runtime.chat_message(lead["id"], "broadcast", "Policy update only", "before-finish"), first)
                self.assertEqual(self.event(child, "before-finish")["status"], "stored_only")
            self.runtime.dispatch()
            self.assertTrue(archive_started.wait(5), "Parent receipt did not schedule reviewer archival")
            before_archive = self.runtime.agent(child["id"])
            self.assertEqual(before_archive["status"], "completed")
            self.assertEqual(before_archive["lastCompletedTurnStatus"], "completed")
            self.assertTrue(before_archive.get("reviewArchiveScheduled"))
            archive_release.set()
            self.assertTrue(archive_finished.wait(5), "Reviewer archive did not finish")

        archived = self.runtime.agent(child["id"])
        self.assertEqual(archived["status"], "paused")
        self.assertFalse(archived["autoWake"])
        self.assertEqual(archived["lastCompletedTurnStatus"], "completed")
        self.assertTrue(archived.get("agentArchive"))
        self.assertFalse(any(method == "turn/start" and "Policy update only" in json.dumps(params)
                             for method, params in self.runtime.server.calls))

    def test_direct_followup_keeps_next_turn_and_broadcast(self):
        lead = self.lead()
        child = self.worker(lead)
        with self.runtime.lock:
            self.runtime.chat_message(lead["id"], "broadcast", "Policy update", "mixed-broadcast")
            self.runtime.chat_message(lead["id"], child["id"], "Review the next file", "direct-work")
            self.finish(child)
            self.assertEqual(self.runtime.agent(child["id"])["status"], "queued")
            self.assertEqual(self.event(child, "mixed-broadcast")["status"], "pending")
        fixture.eventually(lambda: self.runtime.agent(child["id"])["status"] == "running")
        starts = [json.dumps(p) for m, p in self.runtime.server.calls if m == "turn/start" and p["threadId"] == child["threadId"]]
        self.assertTrue(any("Policy update" in p and "Review the next file" in p for p in starts))

    def test_child_result_still_wakes_lead_with_pending_broadcast(self):
        lead = self.lead()
        child = self.worker(lead)
        with self.runtime.lock:
            self.runtime.chat_message(child["id"], "broadcast", "Shared finding", "child-broadcast")
            self.finish(child)
            self.finish(lead)
            self.assertEqual(self.runtime.agent(lead["id"])["status"], "queued")
            self.assertEqual(self.event(lead, "child-broadcast")["status"], "pending")
        fixture.eventually(lambda: self.runtime.agent(lead["id"])["status"] == "running")
        starts = [json.dumps(p) for m, p in self.runtime.server.calls if m == "turn/start" and p["threadId"] == lead["threadId"]]
        self.assertTrue(any("child_result" in p and "Shared finding" in p for p in starts))

    def test_active_monitor_keeps_broadcast_delivery(self):
        lead = self.lead()
        child = self.worker(lead)
        with self.runtime.lock:
            with self.runtime.db() as db:
                self.runtime.put(db, "monitors", {"id": "fixture-watch", "agent": child["id"], "status": "running"})
            self.runtime.chat_message(lead["id"], "broadcast", "Build policy", "active-monitor")
            self.finish(child)
            self.assertEqual(self.runtime.agent(child["id"])["status"], "queued")
            self.assertEqual(self.event(child, "active-monitor")["status"], "pending")
            with self.runtime.db() as db:
                db.execute("DELETE FROM runtime_monitors WHERE id='fixture-watch'")

    def test_active_children_keep_lead_broadcast_delivery(self):
        lead = self.lead()
        child = self.worker(lead)
        with self.runtime.lock:
            self.runtime.chat_message(child["id"], "broadcast", "Review policy", "active-child")
            self.finish(lead)
            self.assertEqual(self.runtime.agent(lead["id"])["status"], "queued")
            self.assertEqual(self.event(lead, "active-child")["status"], "pending")

    def test_old_epoch_work_does_not_reactivate_current_broadcast(self):
        lead = self.lead()
        child = self.worker(lead)
        with self.runtime.lock:
            self.runtime.chat_message(lead["id"], child["id"], "Obsolete direct request", "old-direct")
            with self.runtime.db() as db:
                db.execute("UPDATE runtime_events SET epoch=-1 WHERE id=?", (f"chat:old-direct:{child['id']}",))
            self.runtime.chat_message(lead["id"], "broadcast", "Policy update", "current-broadcast")
            self.finish(child)
            self.assertEqual(self.runtime.agent(child["id"])["status"], "completed")
            self.assertEqual(self.event(child, "current-broadcast")["status"], "stored_only")
            self.assertEqual(self.event(child, "old-direct")["status"], "pending")


if __name__ == "__main__":
    unittest.main()
