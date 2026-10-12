#!/usr/bin/env python3
"""Scheduler roster reads stay scoped and refresh after agent writes."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_waits import HANG_GUARD_SECONDS
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location("scheduler_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class SchedulerDecode(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.tmp.name), fixture.FakeServer)
        self.lead = self.runtime.create({"name": "Lead", "cwd": self.tmp.name,
                                         "prompt": "Work"}, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.lead["id"], db)
            agent["status"] = "running"
            self.runtime.put(db, "agents", agent)
        self.runtime.closed = True
        self.runtime.changed.set()
        self.runtime.scheduler.join(timeout=HANG_GUARD_SECONDS)
        self.assertFalse(self.runtime.scheduler.is_alive())
        self.runtime.closed = False

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def dispatch_with_roster(self, after_read=None, between_sessions=None):
        calls, full_reads = [], []
        original_roster = self.runtime.scheduler_agents
        original_records = self.runtime.records

        def roster(db):
            agents = original_roster(db)
            calls.append((db, self.runtime.lock._is_owned(),
                          [(a["name"], a["status"]) for a in agents]))
            if after_read:
                after_read(len(calls), db)
            return agents

        def records(db, table, **kwargs):
            if table == "agents" and self.runtime.lock._is_owned():
                full_reads.append(table)
            return original_records(db, table, **kwargs)

        from codex_session_names import session_names
        with patch.object(self.runtime, "scheduler_agents", side_effect=roster), \
                patch.object(self.runtime, "records", side_effect=records), \
                patch.object(session_names(self.runtime), "tick", lambda: None), \
                patch("codex_context_repair.tick_restart_input_waits",
                      side_effect=between_sessions or (lambda *_: None)):
            self.runtime.dispatch_all()
        return calls, full_reads

    def test_reuses_one_roster_across_unchanged_database_sessions(self):
        calls, full_reads = self.dispatch_with_roster()
        # The one full decode now happens on the unlocked snapshot connection;
        # the matching locked key proof reuses it without another decode.
        self.assertEqual(len(calls), 1)
        self.assertFalse(calls[0][1])
        self.assertEqual(full_reads, [])

    def test_direct_agent_update_refreshes_within_lock_session(self):
        def update_agent(call_number, db):
            if call_number == 1:
                # A second writable connection changes the row after the
                # unlocked snapshot decode, forcing a fresh locked roster.
                with self.runtime.lock, self.runtime.db() as writer:
                    writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval') "
                                   "WHERE id=?", (self.lead["id"],))
            elif call_number == 2:
                # Simulate an in-session writer after the locked key check and
                # roster decode, before that list is handed to later consumers.
                key = self.runtime.scheduler_roster_key(db)
                db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','parked') "
                           "WHERE id=?", (self.lead["id"],))
                self.assertNotEqual(key, self.runtime.scheduler_roster_key(db))

        calls, full_reads = self.dispatch_with_roster(after_read=update_agent)
        self.assertEqual(calls[0][2], [("Lead", "running")])
        self.assertEqual(calls[1][2], [("Lead", "approval")])
        self.assertEqual(calls[2][2], [])
        self.assertIs(calls[1][0], calls[2][0])
        self.assertEqual(calls[3][2], [])
        # Four full decodes are expected: unlocked snapshot, locked refresh
        # after the outside writer, same-session refresh after recovery, and
        # dispatch_candidates' new-session read after the in-session write.
        self.assertEqual(len(calls), 4)
        self.assertEqual(self.runtime.agent(self.lead["id"])["status"], "parked")
        self.assertEqual(full_reads, [])

    def test_agent_write_between_lock_sessions_refreshes_roster(self):
        def update_agent(_runtime, _agents):
            with self.runtime.lock, self.runtime.db() as db:
                agent = self.runtime.agent(self.lead["id"], db)
                agent["status"] = "approval"
                self.runtime.put(db, "agents", agent)

        calls, full_reads = self.dispatch_with_roster(between_sessions=update_agent)
        self.assertEqual(calls[0][2], [("Lead", "running")])
        self.assertIn(("Lead", "approval"), [agent for _, _, agents in calls[1:] for agent in agents])
        self.assertEqual(full_reads, [])


if __name__ == "__main__":
    unittest.main()
