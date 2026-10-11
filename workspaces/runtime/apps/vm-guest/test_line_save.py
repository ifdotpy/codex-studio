"""line.save runs the layr commands that the vendored layr has (docs/vm-layr.md)."""
from __future__ import annotations

import asyncio
import json
from pathlib import Path
import tempfile
import unittest
import uuid

import layr_agents


class LineSave(unittest.TestCase):
    def test_a_turn_save_repairs_caches_then_saves(self):
        with tempfile.TemporaryDirectory() as folder:
            folder = Path(folder).resolve()
            state, root = folder / "state", folder / "layr"
            project, agent = "a" * 64, str(uuid.uuid4())
            (root / "projects" / project / "lines" / "main").mkdir(parents=True)
            handlers = layr_agents.AgentHandlers(state, root)
            (handlers.associations / (agent + ".json")).write_text(json.dumps(
                {"agentId": agent, "projectId": project, "line": "main", "subpath": ".",
                 "readOnly": False, "owner": "studio-p-x", "uid": 2000, "gid": 2000, "home": "/home/x"}))
            calls = []

            async def layr(context, *args, root=False):
                calls.append(args)
                return "state-1"

            handlers.layr = layr

            async def emit(*_):
                pass

            result = asyncio.run(handlers.dispatch_locked(
                "request", "line.save", {"agentId": agent, "turnId": "turn-1"}, emit))
        self.assertEqual(result, {"stateId": "state-1", "turnId": "turn-1"})
        # layr renamed its "layer" command to "cache"; the old name fails every turn save.
        self.assertEqual(calls[0], ("cache", "repair"))
        self.assertEqual(calls[1][:2], ("save", "--force"))


if __name__ == "__main__":
    unittest.main()
