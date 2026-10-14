#!/usr/bin/env python3
"""Native operation edges retain named refusals until their owner is connected."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment
import os
from pathlib import Path
import sys
import tempfile
import unittest

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from http_backend_harness import BackendProcess, TimedHTTPContractTest, no_write_snapshot
from runtime_test_environment import make_runtime


class NativeHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = BackendProcess(Path(os.environ["TMPDIR"]))
        homes = tempfile.TemporaryDirectory(prefix="p11-http-homes-", dir=os.environ["XDG_CACHE_HOME"])
        cls.addClassCleanup(homes.cleanup)
        cls.addClassCleanup(cls.backend.close)
        for field in ("home", "codex_home", "claude_home", "xdg_cache"):
            directory = Path(homes.name) / field
            directory.mkdir()
            setattr(cls.backend, field, directory)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        from codex_runtime import Runtime
        runtime = make_runtime(Runtime, cls.backend)
        cls.ids = {}
        try:
            for name in ("empty", "held"):
                agent = runtime.create(dict(name=name, cwd=str(cls.backend.cwd), prompt=""), draft=True, defer=True)
                agent.update(autoWake=False, status="paused", inFlight=False)
                if name == "held":
                    agent["contextRepair"] = dict(id="repair", phase="unknown")
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "agents", agent)
                cls.ids[name] = agent["id"]
        finally:
            runtime.close()
        cls.backend.start()

    def headers(self):
        return {"Origin": self.backend.origin or "", "X-Canvas-Token": self.backend.session["token"], "X-Studio-API-Schema": self.backend.schema_hash}

    def post(self, path, body):
        return self.backend.request("POST", path, body=body, headers=self.headers())

    def test_repair_noop_and_unknown_hold(self):
        for name, status in (("empty", 200), ("held", 400)):
            before = no_write_snapshot(self.backend.state)
            response = self.post("/api/context-repair", {"id": self.ids[name]})
            self.assertEqual(response.status, status, response.body)
            if status == 200:
                self.assertEqual(response.json(), {"id": self.ids[name], "repair": None})
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_missing_consumers_refuse_without_writes(self):
        cases = [
            ("/api/action", {"id": self.ids["empty"], "action": action, "request_id": "native-" + action}, "account-worker/native-action")
            for action in ("review", "compact")
        ] + [
            ("/api/branch", {"agent": self.ids["empty"], "message_id": "missing", "id": "branch-request"}, "provider-session/thread-fork"),
            ("/api/checkpoint/restore", {"agent": self.ids["empty"], "checkpoint": "missing", "expectedTree": "tree"}, "provider-session/thread-fork"),
        ]
        for path, body, dependency in cases:
            with self.subTest(path=path, action=body.get("action")):
                before = no_write_snapshot(self.backend.state)
                response = self.post(path, body)
                if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
                    self.assertEqual(response.status, 501, response.body)
                    self.assertEqual(response.json(), {"error": "Not implemented: requires " + dependency})
                else:
                    self.assertEqual(response.status, 400, response.body)
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_command_lookup_is_scoped_and_writes_nothing(self):
        # P18 owns the route's stable-request-identity gate. This frozen base
        # checks missing tasks before its input/cancel branches.
        for action in ("input", "cancel"):
            before = no_write_snapshot(self.backend.state)
            response = self.post("/api/native-command", {"id": "missing-task", "action": action, "text": "input"})
            self.assertEqual(response.status, 404 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 400, response.body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_schema_and_authorization_reject_before_write(self):
        before = no_write_snapshot(self.backend.state)
        self.assertEqual(self.post("/api/action", {"id": self.ids["empty"], "action": "invalid"}).status, 400)
        response = self.backend.request("POST", "/api/context-repair", body={"id": self.ids["empty"]}, headers={"Origin": self.backend.origin or ""})
        self.assertIn(response.status, (401, 403))
        self.assertEqual(no_write_snapshot(self.backend.state), before)


if __name__ == "__main__":
    unittest.main()
