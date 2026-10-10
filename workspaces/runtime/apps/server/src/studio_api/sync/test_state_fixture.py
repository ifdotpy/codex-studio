"""Exercise entity sync against isolated UI fixtures."""

from __future__ import annotations

import json
import os
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from typing import cast
from urllib.error import HTTPError
from urllib.request import Request, urlopen
from uuid import uuid4


class StateFixtureResponseTests(unittest.TestCase):
    fixture_name = "simple-ui-fixture.py"
    process: subprocess.Popen[str] | None = None
    temporary: tempfile.TemporaryDirectory[str] | None = None
    port: int

    @classmethod
    def setUpClass(cls) -> None:
        repository = Path(__file__).resolve().parents[3]
        cache = Path(os.environ.get("TMPDIR", "/tmp"))
        cls.temporary = tempfile.TemporaryDirectory(prefix="sync-entities-fixture-", dir=cache)
        environment = os.environ.copy()
        environment["PYTHONPATH"] = os.pathsep.join((str(repository / "scripts"), str(repository / "tests")))
        cls.process = subprocess.Popen(
            [sys.executable, "-B", str(repository / "tests" / cls.fixture_name),
             str(Path(cls.temporary.name) / "state"), "0"],
            cwd=repository, env=environment, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, bufsize=1,
        )
        output = cls.process.stdout
        if output is None:
            cls._stop_fixture()
            raise RuntimeError("The isolated entity fixture did not expose startup output")
        lines: queue.Queue[str] = queue.Queue()

        def collect_output() -> None:
            for line in output:
                lines.put(line)

        threading.Thread(target=collect_output, name="entity-fixture-output", daemon=True).start()
        observed: list[str] = []
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if cls.process.poll() is not None and lines.empty():
                break
            try:
                line = lines.get(timeout=0.2).strip()
            except queue.Empty:
                continue
            observed.append(line)
            if line.isdecimal():
                cls.port = int(line)
                return
        cls._stop_fixture()
        raise RuntimeError("The isolated entity fixture failed to start: " + " | ".join(observed[-8:]))

    @classmethod
    def tearDownClass(cls) -> None:
        cls._stop_fixture()

    @classmethod
    def _stop_fixture(cls) -> None:
        if cls.process is not None:
            if cls.process.poll() is None:
                cls.process.send_signal(signal.SIGINT)
                try:
                    cls.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    cls.process.kill()
                    cls.process.wait(timeout=5)
            if cls.process.stdout is not None:
                cls.process.stdout.close()
            cls.process = None
        if cls.temporary is not None:
            cls.temporary.cleanup()
            cls.temporary = None

    def get(self, path: str) -> tuple[int, dict[str, object]]:
        try:
            response = urlopen(f"http://127.0.0.1:{self.port}{path}", timeout=20)
        except HTTPError as error:
            response = error
        with response:
            body = response.read()
            value = json.loads(body) if body else {}
            return response.status, value if isinstance(value, dict) else {}

    def entities(self) -> tuple[str, dict[str, dict[str, object]]]:
        status, session = self.get("/api/session")
        self.assertEqual(status, 200)
        token = str(session["token"])
        status, pull = self.get("/api/sync/pull?scope=state:entities:v1&limit=500")
        self.assertEqual(status, 200)
        result: dict[str, dict[str, object]] = {}
        documents = cast(list[dict[str, object]], pull["documents"])
        for document in documents:
            entity = json.loads(cast(str, document["payload"]))
            if not document["_deleted"]:
                result[f"{entity['collection']}:{entity['id']}"] = entity["value"]
        return token, result

    def test_entities_are_available_and_legacy_state_interfaces_are_removed(self) -> None:
        _, entities = self.entities()
        self.assertTrue(any(key.startswith("agent:") for key in entities))
        self.assertIn("workspace:current", entities)
        self.assertEqual(self.get("/api/state")[0], 404)
        for scope in ("state", "state:chat"):
            status, response = self.get("/api/sync/pull?scope=" + scope)
            self.assertEqual(status, 400)
            self.assertEqual(response, {"error": "Invalid sync scope"})


class SidebarProjectStateFixtureTests(StateFixtureResponseTests):
    fixture_name = "sidebar-drag-fixture.py"

    def test_project_and_peer_team_entities_are_current(self) -> None:
        _, entities = self.entities()
        projects = [value for key, value in entities.items() if key.startswith("project:")]
        teams = [value for key, value in entities.items() if key.startswith("peerTeam:")]
        project = next(item for item in projects if item.get("peerTeams"))
        self.assertIsInstance(project.get("updated"), int | float)
        peer_ids = cast(list[dict[str, object] | str], project["peerTeams"])
        self.assertTrue(peer_ids)
        peer_id = peer_ids[0]["id"] if isinstance(peer_ids[0], dict) else peer_ids[0]
        team = next(item for item in teams if item.get("id") == peer_id)
        self.assertEqual(team.get("revision"), 1)


class RenamedAgentStateFixtureTests(StateFixtureResponseTests):
    """Exercise manual-name entity fields after a rename."""

    def test_renamed_lead_entity_is_current(self) -> None:
        token, _ = self.entities()
        assert self.temporary is not None
        project_path = str((Path(self.temporary.name) / "manual-name-project").resolve())
        Path(project_path).mkdir()

        def post(path: str, body: dict[str, object]) -> None:
            request = Request(
                f"http://127.0.0.1:{self.port}{path}",
                data=json.dumps(body).encode(),
                headers={"Content-Type": "application/json", "X-Canvas-Token": token,
                         "Origin": f"http://127.0.0.1:{self.port}"},
                method="POST",
            )
            with urlopen(request, timeout=20) as response:
                self.assertEqual(response.status, 200)

        post("/api/projects", {"path": project_path})
        agent_id = str(uuid4())
        post("/api/leads", {"id": agent_id, "cwd": project_path})
        post("/api/rename", {"id": agent_id, "name": "Evidence reviewer"})
        _, entities = self.entities()
        renamed = entities[f"agent:{agent_id}"]
        self.assertEqual(renamed.get("name"), "Evidence reviewer")
        self.assertIs(renamed.get("manualName"), True)


if __name__ == "__main__":
    unittest.main()
