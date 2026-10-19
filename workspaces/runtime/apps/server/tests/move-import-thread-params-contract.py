#!/usr/bin/env python3
"""A move target builds resume parameters from the incoming record before it saves it."""

from test_isolation import isolate_supervisor_environment

import copy
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
import uuid

# Support direct execution without runner-provided PYTHONPATH.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
isolate_supervisor_environment()
from runtime_test_environment import make_runtime

from codex_agent_move import import_thread_params

spec = importlib.util.spec_from_file_location(
    "move_import_fixture", Path(__file__).with_name("worker-defaults-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

THREAD = "native-thread-from-source"
HISTORY = Path("/moves/incoming/history.jsonl")


class MoveImportThreadParams(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="move-import-params-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.runtime = make_runtime(fixture.ControlledRuntime, self.root / "state", fixture.f.FakeServer)
        self.addCleanup(self.runtime.close)
        self.runtime.catalog = lambda account="default": copy.deepcopy(fixture.CATALOG)
        self.lead = self.runtime.new_lead({"cwd": str(self.root)})

    def incoming(self, source):
        """Return a source record as the target sees it in a move descriptor."""
        lead_tools = self.runtime.tool_definitions(self.runtime.agent(self.lead["id"]))
        self.assertIn("orchestration_spawn", {tool["name"] for tool in lead_tools})
        agent = copy.deepcopy(source)
        new_id = str(uuid.uuid4())
        if agent.get("isLead"):
            agent["rootId"] = new_id
        agent.update(id=new_id, threadId=THREAD, provider="codex", frozenNativeParams={
            "baseInstructions": "Exported base instructions",
            "developerInstructions": "Exported developer instructions",
            "model": "gpt-6-astra",
            # The exported catalog has the spawn tool. Policy decides if it stays.
            "dynamicTools": lead_tools,
        })
        return agent

    def save(self, agent):
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", agent)
        return self.runtime.agent(agent["id"])

    def check_import(self, agent, spawn_allowed):
        # The tool catalog reads the saved record for the delegation policy.
        with self.assertRaisesRegex(ValueError, "Unknown managed agent"):
            self.runtime.tool_definitions(agent)
        params = import_thread_params(self.runtime, agent, THREAD, HISTORY)
        self.assertNotIn("dynamicTools", params)
        self.assertEqual((params["threadId"], params["path"], params["excludeTurns"]),
                         (THREAD, str(HISTORY), True))
        self.assertEqual(params["developerInstructions"], "Exported developer instructions")

        saved = self.save(agent)
        resumed = self.runtime.new_thread_params(saved)
        names = {tool["name"] for tool in resumed.pop("dynamicTools")}
        self.assertEqual("orchestration_spawn" in names, spawn_allowed)
        self.assertEqual(names - {"orchestration_spawn"},
                         {tool["name"] for tool in agent["frozenNativeParams"]["dynamicTools"]}
                         - {"orchestration_spawn"})
        resumed.update(threadId=THREAD, path=str(HISTORY), excludeTurns=True)
        self.assertEqual(params, resumed)

    def test_moved_worker_imports_and_keeps_the_saved_depth_limit(self):
        worker = self.runtime.spawn_agents(self.lead, {"agents": [{
            "name": "Moved worker", "prompt": "Continue", "role": "implementer",
        }]}, "moved-worker")["agents"][0]
        incoming = self.incoming(self.runtime.agent(worker["id"]))
        self.check_import(incoming, spawn_allowed=False)

    def test_moved_lead_imports_and_keeps_its_spawn_tool(self):
        incoming = self.incoming(self.runtime.agent(self.lead["id"]))
        self.check_import(incoming, spawn_allowed=True)


if __name__ == "__main__":
    unittest.main()
