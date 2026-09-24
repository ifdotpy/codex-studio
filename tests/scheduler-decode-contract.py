#!/usr/bin/env python3
"""One scheduler pass decodes agent records again only after a table write."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import Runtime, write_generation
from codex_sync import SyncStore

spec = importlib.util.spec_from_file_location("scheduler_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class SchedulerDecode(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.tmp.name), fixture.FakeServer)
        self.runtime.create({"name": "Lead", "cwd": self.tmp.name, "prompt": "Work"}, defer=True)

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def decodes(self, write=None):
        calls, records = [], Runtime.records

        def counted(db, table):
            if table == "agents":
                calls.append(table)
                if write and len(calls) == 1:
                    write(db)
            return records(db, table)
        from codex_session_names import session_names
        # Count only this pass; the session name scan runs once per second.
        with patch.object(self.runtime, "records", side_effect=counted), \
                patch.object(session_names(self.runtime), "tick", lambda: None):
            self.runtime.dispatch()
        return len(calls)

    def test_generation_reuses_the_list_until_a_write(self):
        with self.runtime.db() as db:
            self.assertIsNone(write_generation(db))
        self.assertEqual(self.decodes(), 3)
        SyncStore(self.runtime.db, lambda: {}, lambda key: {})
        self.assertEqual(self.decodes(), 1)
        # A write after the first decode makes the next read decode again.
        self.assertEqual(self.decodes(lambda db: db.execute(
            "UPDATE runtime_agents SET record=record")), 2)

if __name__ == "__main__":
    unittest.main()
