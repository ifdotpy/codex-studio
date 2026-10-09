#!/usr/bin/env python3
"""Broadcast sends and room projections use only the sender team's live roster."""
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

spec = importlib.util.spec_from_file_location("broadcast_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class BroadcastScope(unittest.TestCase):
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

    def test_broadcast_receipt_does_not_decode_unrelated_agents(self):
        lead = self.runtime.create({"name": "Lead", "cwd": self.tmp.name, "prompt": "Work"}, defer=True)
        lead.update(autoWake=True, status="idle")
        child = dict(lead)
        child.update(id="worker-for-scoped-broadcast", parentId=lead["id"], isLead=False,
                     name="Worker", role="reviewer", prompt="Review", status="paused", autoWake=False)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", lead)
            self.runtime.put(db, "agents", child)
        with self.runtime.lock, patch.object(self.runtime, "records",
                                             side_effect=AssertionError("broadcast decoded every agent")):
            receipt = self.runtime.chat_message(lead["id"], "broadcast", "Progress", "scoped-broadcast")
        self.assertEqual(receipt["deliveries"], {child["id"]: "stored_only"})
        with self.runtime.db() as db:
            indexes = {row[1] for row in db.execute("PRAGMA index_list('runtime_agents')")}
            self.assertIn("runtime_agent_root", indexes)


if __name__ == "__main__":
    unittest.main()
