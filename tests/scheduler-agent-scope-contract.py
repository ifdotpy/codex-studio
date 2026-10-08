#!/usr/bin/env python3
"""Scheduler roster omits archived rows only when no scheduled cleanup needs them."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

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
        # Remove malformed fixture rows before close checks the current API schema.
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("DELETE FROM runtime_agents WHERE id IN ('old-safety-retry','malformed-start-turn')")
        self.runtime.close()
        self.tmp.cleanup()

    def terminal_agents(self, root):
        markers = {
            **{"restart-" + stage: {"restartRecovery": {"stage": stage}} for stage in
               ("finished", "continued", "reattached", "input_restored")},
            "disconnect": {"disconnectRecovery": {"source": "restart", "autoWake": True}},
            **{"repair-" + phase: {"contextRepair": {"phase": phase}} for phase in
               ("unchanged", "completed", "failed")},
            **{"last-wait-" + status: {"lastContextRepairWait": {"status": status}} for status in
               ("resumed", "superseded")},
            "accepted-start": {"startAttempt": {"id": "accepted", "submitted": True,
                "epoch": root["epoch"], "accountKey": "default", "events": ["delivered-event"],
                "turnId": "completed-turn"}},
        }
        return [dict(root, id=status + ":" + name, rootId=root["id"], parentId=root["id"],
                     isLead=False, name=name, status=status, autoWake=False, inFlight=False,
                     turnId=None, threadId="thread", lastCompletedTurn="completed-turn",
                     lastCompletedTurnStatus="completed", **fields)
                for status in ("completed", "paused", "waiting", "parked")
                for name, fields in markers.items()]

    def test_terminal_receipts_do_not_select_idle_agents(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        actors = self.terminal_agents(root)
        with self.runtime.lock, self.runtime.db() as db:
            for actor in actors:
                self.runtime.put(db, "agents", actor)
            selected = {a["id"] for a in self.runtime.scheduler_agents(db)}
        self.assertEqual({a["id"] for a in actors} & selected, set())

    def test_terminal_receipts_are_ignored_by_recovery_consumers(self):
        from codex_browser_recovery import tick as browser_tick
        from codex_connection_recovery import tick as connection_tick, eligible
        from codex_context_repair import recover_context_failures, tick_restart_input_waits
        from codex_tool_response_recovery import tick as tool_tick
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        actors = self.terminal_agents(root)
        self.assertFalse(any(eligible(a) for a in actors))
        with patch.object(self.runtime, "put") as write, \
                patch.object(self.runtime.pool, "submit") as work, \
                patch.object(self.runtime.recovery_pool, "submit") as recovery:
            with self.runtime.lock, self.runtime.db() as db:
                recover_context_failures(self.runtime, db, actors)
                self.assertEqual(self.runtime.release_failed_work(db, actors, force=True), [])
                browser_tick(self.runtime, db, actors)
            self.runtime.queue_turn_recovery(actors)
            connection_tick(self.runtime, actors)
            tool_tick(self.runtime, actors)
            tick_restart_input_waits(self.runtime, actors)
            write.assert_not_called()
            work.assert_not_called()
            recovery.assert_not_called()

    def test_unfinished_and_unknown_receipts_remain_selected(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        completed = {"id": "accepted", "submitted": True, "epoch": root["epoch"],
                     "accountKey": "default", "events": ["event"], "turnId": "completed-turn"}
        markers = {
            **{"restart-" + stage: {"restartRecovery": {"stage": stage, "autoWake": True}}
               for stage in ("pending", "held", "superseded", "future-stage")},
            "preparing": {"contextRepair": {"phase": "preparing"}},
            "unknown-repair": {"contextRepair": {"phase": "future-phase"}},
            "failed-repair": {"status": "failed", "contextRepair": {"phase": "failed"}},
            "unchanged-failed-repair": {"status": "failed", "contextRepair": {"phase": "unchanged"}},
            "unknown-start": {"startAttempt": {**completed, "executionOutcome": "unknown"}},
            "response-error-start": {"startAttempt": {**completed, "responseError": "Outcome unknown"}},
            "unsent-start": {"startAttempt": {**completed, "submitted": False}},
            "missing-submitted-start": {"startAttempt": {k: v for k, v in completed.items() if k != "submitted"}},
            "different-start-turn": {"startAttempt": {**completed, "turnId": "other-turn"}},
            "no-start-turn": {"startAttempt": {k: v for k, v in completed.items() if k != "turnId"}},
            "held-start": {"startAttempt": completed, "startOutcomeHold": {"stage": "held"}},
            "busy-start": {"startAttempt": {**completed, "activeAtReservation": True}},
            "action-start": {"startAttempt": {**completed, "action": "compact"}},
            "unknown-last-wait": {"lastContextRepairWait": {"status": "future-status"}},
            "unknown-disconnect": {"disconnectRecovery": {"stage": "future-stage"}},
            "unknown-cleanup": {"contextRepair": {"phase": "completed",
                "sourceCleanup": {"phase": "future-stage"}}},
            "failed-start-turn": {"startAttempt": completed, "lastCompletedTurnStatus": "failed"},
            "missing-terminal-turn": {"startAttempt": completed, "lastCompletedTurn": None},
            "missing-terminal-status": {"startAttempt": completed, "lastCompletedTurnStatus": None},
            "malformed-start-turn": {"startAttempt": {**completed, "turnId": 7}, "lastCompletedTurn": 7},
            "context-wait": {"contextRepairWait": {"stage": "pending"}},
            "budget-start": {"budgetStartWait": {"stage": "waiting"}},
            "budget-action": {"budgetActionWait": {"action": "capacity"}},
        }
        for phase in ("planned", "submitted"):
            for deleted in (False, True):
                markers["cleanup-" + phase + str(deleted)] = {
                    "contextRepair": {"phase": "completed", "sourceCleanup": {"phase": phase}},
                    "deletedAt": time.time() if deleted else None}
        with self.runtime.lock, self.runtime.db() as db:
            for key, fields in markers.items():
                actor = dict(root, id=key, rootId=root["id"], parentId=root["id"], isLead=False,
                             name=key, status="completed", autoWake=False, inFlight=False,
                             turnId=None, threadId="thread", lastCompletedTurn="completed-turn",
                             lastCompletedTurnStatus="completed")
                actor.update(fields)
                if key == "malformed-start-turn":
                    db.execute("INSERT INTO runtime_agents VALUES (?,?)", (key, json.dumps(actor)))
                else:
                    self.runtime.put(db, "agents", actor)
            selected = {a["id"] for a in self.runtime.scheduler_agents(db)}
        self.assertEqual(set(markers) - selected, set())

    def test_work_owner_selection_keeps_failed_and_deleted_cleanup(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        expected = {"failed", "deleted"}
        with self.runtime.lock, self.runtime.db() as db:
            for status in ("completed", "paused", "waiting", "parked", "failed", "deleted"):
                actor = dict(root, id=status, rootId=root["id"], parentId=root["id"], isLead=False,
                             name=status, status="completed" if status == "deleted" else status,
                             autoWake=False, inFlight=False,
                             deletedAt=time.time() if status == "deleted" else None)
                self.runtime.put(db, "agents", actor)
                self.runtime.put(db, "work", {"id": "work-" + status, "rootId": root["id"],
                    "title": status, "version": 1, "owner": status,
                    "status": "running", "dependencies": []})
            roster = self.runtime.scheduler_agents(db)
            selected = {a["id"] for a in roster}
            self.assertEqual(selected - {root["id"]}, expected)
            self.assertEqual({w["owner"] for w in self.runtime.records(db, "work")},
                             {"completed", "paused", "waiting", "parked", "failed", "deleted"})
            self.assertEqual(set(self.runtime.release_failed_work(db, roster, force=True)),
                             {"work-failed", "work-deleted"})
            owners = {w["id"]: w["owner"] for w in self.runtime.records(db, "work")}
            self.assertEqual(owners, {"work-" + status: None if status in expected else status
                                     for status in ("completed", "paused", "waiting", "parked", "failed", "deleted")})

    def test_pending_transfer_keeps_the_whole_root_roster(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_account_transfers (id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            for status in ("completed", "paused", "waiting", "parked"):
                actor = dict(root, id=status, rootId=root["id"], parentId=root["id"], isLead=False,
                             name=status, status=status, autoWake=False, inFlight=False)
                self.runtime.put(db, "agents", actor)
            self.runtime.put(db, "account_transfers", {"id": "pending-transfer",
                "leadId": root["id"], "status": "pending"})
            selected = {a["id"] for a in self.runtime.scheduler_agents(db)}
        self.assertEqual(selected, {root["id"], "completed", "paused", "waiting", "parked"})

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
        repair.update(id="archived-repair-owner", name="Repair owner",
                      contextRepairWait={"stage": "pending"})
        stale = dict(archived)
        stale.update(id="archived-stale", name="Archived stale", restartRecovery={"stage": "finished"})
        with self.runtime.lock, self.runtime.db() as db:
            for agent in (live, archived, owner, repair, stale):
                self.runtime.put(db, "agents", agent)
            db.execute("INSERT INTO runtime_work(id,record) VALUES (?,?)", (
                "archived-work", '{"id":"archived-work","owner":"archived-work-owner","status":"ready"}'))
            selected = {agent["id"] for agent in self.runtime.scheduler_agents(db)}
        self.assertIn("live-worker", selected)
        self.assertIn("archived-work-owner", selected)
        self.assertIn("archived-repair-owner", selected)
        self.assertNotIn("archived-worker", selected)
        self.assertNotIn("archived-stale", selected)
        self.assertEqual(set(self.runtime._scheduler_agent_cache), selected)

    def test_every_recovery_and_wait_marker_is_retained(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        markers = {
            "interrupted-connection": {"status": "interrupted", "threadId": "thread", "turnId": "turn"},
            "codex-disconnect-error": {"status": "interrupted", "threadId": "thread", "turnId": "turn",
                "error": "Codex disconnected. Review the transcript before resuming."},
            "claude-disconnect-error": {"status": "interrupted", "threadId": "claude-thread",
                "turnId": "claude-turn", "provider": "claude", "error": "Connection lost"},
            "server-restart-error": {"status": "interrupted", "threadId": "server-thread",
                "turnId": "server-turn", "error": "Server restarted during a turn"},
            "restart-pending": {"restartRecovery": {"stage": "pending", "autoWake": False}},
            "restart-other-stage": {"restartRecovery": {"stage": "reconciling", "autoWake": False}},
            "disconnect-marker": {"disconnectRecovery": {"stage": "reconciling"}},
            "context-repair": {"contextRepair": {"phase": "unknown-provider-phase"}},
            "browser-recovery": {"browserRecovery": {"stage": "failed"}},
            "capacity-retry": {"capacityRetry": {"status": "failed", "attempt": 7}},
            "usage-resume": {"usageResume": {"status": "waiting", "attempt": 4}},
            "old-safety-retry": {"nativeSafetyRetry": {"stage": "unknown-provider-stage",
                "epoch": -1, "accountKey": "old-account"}},
            "last-context-wait": {"lastContextRepairWait": {"stage": "retired"}},
            "failure-hold": {"nativeFailureHold": True},
            "budget-action-wait": {"budgetActionWait": {"action": "capacity"}},
            "budget-start-wait": {"budgetStartWait": {"stage": "waiting"}},
            "safety-retry": {"nativeSafetyRetry": {"stage": "verify_turns", "epoch": root["epoch"],
                "accountKey": "default"}},
        }
        with self.runtime.lock, self.runtime.db() as db:
            for key, fields in markers.items():
                agent = dict(root, id=key, rootId=root["id"], parentId=root["id"], isLead=False,
                             name=key, status="failed", autoWake=False)
                agent.update(fields)
                if key == "old-safety-retry":
                    # Stored legacy stages can be outside the current API enum.
                    db.execute("INSERT INTO runtime_agents VALUES (?,?)", (key, json.dumps(agent)))
                else:
                    self.runtime.put(db, "agents", agent)
            selected = {agent["id"] for agent in self.runtime.scheduler_agents(db)}
        self.assertEqual(set(markers) - selected, set())

    def test_work_changes_invalidate_deleted_owner_roster(self):
        root = self.runtime.create({"name": "Root", "cwd": self.tmp.name, "prompt": "Coordinate"}, defer=True)
        owner = dict(root, id="archived-work-owner", rootId=root["id"], parentId=root["id"],
                     isLead=False, name="Work owner", status="completed", autoWake=False,
                     deletedAt=time.time())
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", owner)
            self.assertNotIn(owner["id"], {a["id"] for a in self.runtime.scheduler_agents(db)})
        with self.runtime.lock, self.runtime.db() as db:
            work = {"id": "owner-work", "rootId": root["id"], "owner": owner["id"],
                    "status": "ready", "dependencies": []}
            self.runtime.put(db, "work", work)
            self.assertIn(owner["id"], {a["id"] for a in self.runtime.scheduler_agents(db)})
            work["status"] = "accepted"
            self.runtime.put(db, "work", work)
        with self.runtime.db() as db:
            self.assertNotIn(owner["id"], {a["id"] for a in self.runtime.scheduler_agents(db)})


if __name__ == "__main__":
    unittest.main()
