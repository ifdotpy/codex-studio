#!/usr/bin/env python3
"""HTTP contracts for persisted projects, sidebar order, and peer teams."""

from __future__ import annotations

import os
import sqlite3
import sys
import unittest
from contextlib import closing
from pathlib import Path
from unittest.mock import patch

SERVER_SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SERVER_SRC))

from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()


class ProjectsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        from codex_runtime import Runtime

        with patch.dict(os.environ, cls.backend._environment(), clear=True):
            runtime = Runtime(cls.backend.state)
            try:
                project = {
                    "id": str(cls.backend.cwd),
                    "path": str(cls.backend.cwd),
                    "name": "Python written project",
                    "created": 10.0,
                    "accountKey": "default",
                    "accountRevision": 1,
                }
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "projects", project)
            finally:
                runtime.close()
        cls.backend.start()

    @property
    def backend(self):
        return type(self).backend

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def _project_record(self, project_id: str) -> dict:
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            row = db.execute("SELECT record FROM runtime_projects WHERE id=?", (project_id,)).fetchone()
        return {} if row is None else __import__("json").loads(row[0])

    def _project_count(self) -> int:
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            return int(db.execute("SELECT count(*) FROM runtime_projects").fetchone()[0])

    def test_reads_python_written_project_without_loss(self) -> None:
        response = self.backend.request("GET", "/api/projects")
        self.assertEqual(response.status, 200, response.body[:1000])
        project = next(item for item in response.json()["items"] if item["id"] == str(self.backend.cwd))
        self.assertEqual(project["name"], "Python written project")
        self.assertEqual(project["accountKey"], "default")
        self.assertEqual(project["accountRevision"], 1)

    def test_register_and_remove_project_updates_storage_and_sync_projection(self) -> None:
        folder = self.backend.root / "project-register"
        folder.mkdir()
        registered = self.backend.request(
            "POST", "/api/projects", body={"path": str(folder), "name": "Registered"},
            headers=self.headers(),
        )
        self.assertEqual(registered.status, 200, registered.body[:1000])
        project = registered.json()
        self.assertEqual(project["path"], str(folder.resolve()))
        self.assertEqual(project["name"], "Registered")
        self.assertEqual(self._project_record(project["id"])["name"], "Registered")

        removed = self.backend.request(
            "POST", "/api/projects", body={"action": "remove", "path": project["id"]},
            headers=self.headers(),
        )
        self.assertEqual(removed.status, 200, removed.body[:1000])
        self.assertTrue(removed.json()["removed"])
        self.assertEqual(self._project_record(project["id"]), {})
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            entity = db.execute(
                "SELECT deleted FROM sync_entities WHERE collection='project' AND id=?",
                (project["id"],),
            ).fetchone()
        self.assertEqual(entity, (1,))

    def test_invalid_registration_refuses_without_writing(self) -> None:
        before = self._project_count()
        response = self.backend.request(
            "POST",
            "/api/projects",
            body={"path": str(self.backend.root / "missing-project"), "action": "register"},
            headers=self.headers(),
        )
        self.assertIn(response.status, (400, 500), response.body[:1000])
        self.assertEqual(self._project_count(), before)

    def test_sidebar_order_commit_and_exact_retry(self) -> None:
        body = {
            "action": "reorder",
            "request_id": "projects-sidebar-order-1",
            "expected_revision": 0,
            "groups": {"/workspace": ["chat-1", "chat-2"]},
        }
        first = self.backend.request("POST", "/api/projects", body=body, headers=self.headers())
        replay = self.backend.request("POST", "/api/projects", body=body, headers=self.headers())
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(first.json(), replay.json())
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            row = db.execute(
                "SELECT record FROM runtime_sidebar_order WHERE id='current'"
            ).fetchone()
        self.assertEqual(__import__("json").loads(row[0]), first.json())

    def test_host_worker_environment_persists_project_choice(self) -> None:
        folder = self.backend.root / "worker-environment"
        folder.mkdir()
        response = self.backend.request(
            "POST",
            "/api/projects",
            body={
                "action": "set_worker_environment",
                "path": str(folder),
                "environment": "host",
                "expected_revision": 0,
            },
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        project = response.json()
        self.assertEqual(project["workerEnvironment"], "host")
        self.assertEqual(project["workerEnvironmentRevision"], 1)
        self.assertEqual(self._project_record(project["id"])["workerEnvironment"], "host")

    def test_peer_team_save_persists_project_and_peer_team_projection(self) -> None:
        from codex_runtime import Runtime

        self.backend.stop()
        with patch.dict(os.environ, self.backend._environment(), clear=True):
            runtime = Runtime(self.backend.state)
            try:
                with runtime.lock, runtime.db() as db:
                    for agent_id in ("project-peer-a", "project-peer-b"):
                        runtime.put(db, "agents", {
                            "id": agent_id,
                            "rootId": agent_id,
                            "epoch": 0,
                            "status": "idle",
                            "name": agent_id,
                            "role": "orchestrator",
                            "cwd": str(self.backend.cwd),
                            "isLead": True,
                            "autoWake": True,
                            "approvalPolicy": "never",
                        }, sync_rooms=False)
            finally:
                runtime.close()
        self.backend.start()
        response = self.backend.request("POST", "/api/peer-teams", body={
            "action": "save",
            "path": str(self.backend.cwd),
            "expected_revision": 0,
            "team_id": "11111111-1111-4111-8111-111111111111",
            "request_id": "projects-peer-team-1",
            "name": "Reviewers",
            "members": ["project-peer-a", "project-peer-b"],
        }, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        project = response.json()
        self.assertEqual(project["peerTeamsRevision"], 1)
        self.assertEqual(project["peerTeams"][0]["members"], ["project-peer-a", "project-peer-b"])
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            entity = db.execute(
                "SELECT payload,deleted FROM sync_entities WHERE collection='peerTeam' AND id=?",
                ("11111111-1111-4111-8111-111111111111",),
            ).fetchone()
        self.assertIsNotNone(entity)
        self.assertEqual(entity[1], 0)


if __name__ == "__main__":
    unittest.main()
