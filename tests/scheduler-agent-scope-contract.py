#!/usr/bin/env python3
"""Scheduler roster omits archived rows only when no scheduled cleanup needs them."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location("scope_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class SchedulerAgentScope(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.tmp.name), fixture.FakeServer)
        self.runtime.closed = True
        self.runtime.changed.set()
        self.runtime.scheduler.join(timeout=5)
        self.assertFalse(self.runtime.scheduler.is_alive())
        self.runtime.closed = False

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def test_live_rows_and_archived_cleanup_owners_are_retained(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        live = dict(root)
        live.update(id="live-worker", rootId=root["id"], parentId=root["id"], isLead=False,
                    name="Live", status="queued", autoWake=True, deletedAt=None)
        archived = dict(live)
        archived.update(id="archived-worker", name="Archived", deletedAt=time.time(), status="completed")
        owner = dict(archived)
        owner.update(id="archived-work-owner", name="Work owner")
        repair = dict(archived)
        repair.update(id="archived-repair-owner", name="Repair owner", contextRepairWait={"stage": "pending"})
        with self.runtime.lock, self.runtime.db() as db:
            for agent in (live, archived, owner, repair):
                self.runtime.put(db, "agents", agent)
            db.execute("INSERT INTO runtime_work(id,record) VALUES (?,?)", (
                "archived-work", '{"id":"archived-work","owner":"archived-work-owner","status":"ready"}'))
            selected = {agent["id"] for agent in self.runtime.scheduler_agents(db)}
        self.assertIn("live-worker", selected)
        self.assertIn("archived-work-owner", selected)
        self.assertIn("archived-repair-owner", selected)
        self.assertNotIn("archived-worker", selected)

    def test_every_recovery_and_wait_marker_is_retained(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        markers = {
            "interrupted-connection": {"status": "interrupted", "threadId": "thread", "turnId": "turn"},
            "disconnect-marker": {"disconnectRecovery": {"stage": "pending"}},
            "restart-marker": {"restartRecovery": {"stage": "pending"}},
            "failure-hold": {"nativeFailureHold": True},
            "budget-action-wait": {"budgetActionWait": {"action": "capacity"}},
            "budget-start-wait": {"budgetStartWait": {"stage": "waiting"}},
            "capacity-retry": {"capacityRetry": {"status": "scheduled"}},
            "usage-resume": {"usageResume": {"status": "scheduled"}},
            "safety-retry": {"nativeSafetyRetry": {"stage": "verify_turns"}},
        }
        with self.runtime.lock, self.runtime.db() as db:
            for key, fields in markers.items():
                agent = dict(root, id=key, rootId=root["id"], parentId=root["id"], isLead=False,
                             name=key, status="failed", autoWake=False)
                agent.update(fields)
                self.runtime.put(db, "agents", agent)
            selected = {agent["id"] for agent in self.runtime.scheduler_agents(db)}
        self.assertTrue(set(markers) <= selected)


if __name__ == "__main__":
    unittest.main()
