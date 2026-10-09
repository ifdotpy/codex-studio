#!/usr/bin/env python3
"""Scheduler roster reads stay scoped and refresh after agent writes."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
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
        self.runtime.scheduler.join(timeout=5)
        self.assertFalse(self.runtime.scheduler.is_alive())
        self.runtime.closed = False

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def dispatch_with_roster(self, after_first=None, between_sessions=None):
        calls, full_reads = [], []
        original_roster = self.runtime.scheduler_agents
        original_records = self.runtime.records

        def roster(db):
            agents = original_roster(db)
            calls.append((db, self.runtime.lock._is_owned(),
                          [(a["name"], a["status"]) for a in agents]))
            if len(calls) == 1 and after_first:
                after_first(db)
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
        self.assertEqual(len(calls), 1)
        self.assertTrue(all(locked for _, locked, _ in calls))
        self.assertEqual(full_reads, [])

    def test_direct_agent_update_refreshes_within_lock_session(self):
        def update_agent(db):
            db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval') "
                       "WHERE id=?", (self.lead["id"],))

        calls, full_reads = self.dispatch_with_roster(after_first=update_agent)
        self.assertEqual(calls[0][2], [("Lead", "running")])
        self.assertIn(("Lead", "approval"), [agent for _, _, agents in calls[1:] for agent in agents])
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
