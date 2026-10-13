#!/usr/bin/env python3
"""HTTP contracts for persisted projects, sidebar order, and peer teams."""

from __future__ import annotations

import os
import hashlib
import json
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
        stored = self._project_record(project["id"])
        self.assertEqual(stored["name"], "Registered")
        self.assertEqual(stored["homeServerId"], project["homeServerId"])
        self.assertEqual(stored["locations"], [{"serverId": project["homeServerId"], "path": project["path"], "projectId": project["id"]}])
        self.assertEqual(stored["locationsRevision"], 0)

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

    def test_remote_registration_returns_named_501_without_writing(self) -> None:
        before = self._project_count()
        response = self.backend.request("POST", "/api/projects", body={
            "action": "register", "path": str(self.backend.cwd), "server": "remote-server", "request_id": "remote-registration",
        }, headers=self.headers())
        expected = 501 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 403
        self.assertEqual(response.status, expected, response.body[:1000])
        if expected == 501:
            self.assertIn("cross-server-driver/project-registration", response.json()["error"])
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

    def test_set_account_adds_default_to_existing_project_account_keys(self) -> None:
        from codex_runtime import Runtime

        folder = self.backend.root / "account-keys"
        folder.mkdir()
        self.backend.stop()
        with patch.dict(os.environ, self.backend._environment(), clear=True):
            runtime = Runtime(self.backend.state)
            try:
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "projects", {
                        "id": str(folder), "path": str(folder), "name": "Accounts", "created": 10.0,
                        "accountKey": "old", "accountKeys": ["old"], "accountRevision": 1,
                    })
            finally:
                runtime.close()
        self.backend.start()
        response = self.backend.request("POST", "/api/projects", body={
            "action": "set_account", "path": str(folder), "account_key": "default", "expected_revision": 1,
        }, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["accountKey"], "default")
        self.assertEqual(response.json()["accountKeys"], ["default", "old"])

    def test_existing_implicit_host_is_a_noop(self) -> None:
        from codex_runtime import Runtime

        folder = self.backend.root / "implicit-host"
        folder.mkdir()
        self.backend.stop()
        with patch.dict(os.environ, self.backend._environment(), clear=True):
            runtime = Runtime(self.backend.state)
            try:
                project = {"id": str(folder), "path": str(folder), "name": "Implicit host", "created": 10.0, "accountKey": "default", "accountRevision": 1}
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "projects", project)
            finally:
                runtime.close()
        self.backend.start()
        body = {"action": "set_worker_environment", "path": str(folder), "environment": "host", "expected_revision": 0}
        before = self._project_record(str(folder))
        response = self.backend.request("POST", "/api/projects", body=body, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json(), before)

    def test_base_retry_returns_saved_project_and_conflicting_replay_refuses(self) -> None:
        folder = self.backend.root / "worker-base"
        folder.mkdir()
        body = {"action": "set_worker_base", "path": str(folder), "base_ref": "main", "expected_revision": 0}
        first = self.backend.request("POST", "/api/projects", body=body, headers=self.headers())
        replay = self.backend.request("POST", "/api/projects", body=body, headers=self.headers())
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(first.json()["workerBaseRef"], replay.json()["workerBaseRef"])
        self.assertEqual(first.json()["workerBaseRevision"], replay.json()["workerBaseRevision"])
        conflict = self.backend.request("POST", "/api/projects", body={**body, "base_ref": "release"}, headers=self.headers())
        self.assertIn(conflict.status, (400, 409), conflict.body[:1000])

    def test_invalid_names_and_paths_refuse_without_writing(self) -> None:
        before = self._project_count()
        for path, name in ((str(self.backend.root / "blank-name"), "   "), (str(self.backend.root / "nul\0suffix"), "bad"), (" " * 4097, "bad")):
            response = self.backend.request("POST", "/api/projects", body={"action": "register", "path": path, "name": name}, headers=self.headers())
            self.assertIn(response.status, (400, 422), response.body[:1000])
        self.assertEqual(self._project_count(), before)

    def test_missing_suffix_under_symlink_uses_resolved_ancestor(self) -> None:
        target = self.backend.root / "symlink-target"
        alias = self.backend.root / "symlink-alias"
        target.mkdir()
        alias.symlink_to(target, target_is_directory=True)
        path = alias / "not-created"
        response = self.backend.request("POST", "/api/projects", body={
            "action": "set_worker_base", "path": str(path), "base_ref": "main", "expected_revision": 0,
        }, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["path"], str(target / "not-created"))
        self.assertFalse(path.exists())

    def test_project_folder_rename_and_folder_actions_are_persisted(self) -> None:
        folder = self.backend.root / "organization"
        folder.mkdir()
        body = {"action": "rename", "path": str(folder), "name": "  Renamed project  ", "expected_revision": 0}
        renamed = self.backend.request("POST", "/api/projects", body=body, headers=self.headers())
        self.assertEqual(renamed.status, 200, renamed.body[:1000])
        self.assertEqual(renamed.json()["name"], "Renamed project")
        folder_id = "22222222-2222-4222-8222-222222222222"
        added = self.backend.request("POST", "/api/projects", body={"action": "add_folder", "path": str(folder), "name": "Source", "folder_id": folder_id, "expected_revision": 1}, headers=self.headers())
        self.assertEqual(added.status, 200, added.body[:1000])
        self.assertEqual(added.json()["folders"][0]["id"], folder_id)

    def test_sidebar_receipt_uses_python_canonical_signature(self) -> None:
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            raw = db.execute("SELECT record FROM runtime_sidebar_order WHERE id='current'").fetchone()
            current = 0 if raw is None else json.loads(raw[0])["revision"]
        body = {"action": "reorder", "request_id": "python-receipt", "expected_revision": current, "groups": {"/workspace": ["chat-1"]}}
        saved = {"revision": current + 1, "groups": body["groups"]}
        signature = hashlib.sha256(json.dumps({"operation": "sidebar-order", "body": body}, sort_keys=True).encode()).hexdigest()
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            db.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?,?,?)", ("sidebar-order:python-receipt", signature, json.dumps(saved)))
            db.commit()
        response = self.backend.request("POST", "/api/projects", body=body, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json(), saved)

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
        deleted = self.backend.request("POST", "/api/peer-teams", body={
            "action": "delete", "path": str(self.backend.cwd), "expected_revision": 1,
            "team_id": "11111111-1111-4111-8111-111111111111", "request_id": "projects-peer-team-delete",
        }, headers=self.headers())
        self.assertEqual(deleted.status, 200, deleted.body[:1000])
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            tombstone = db.execute(
                "SELECT deleted FROM sync_entities WHERE collection='peerTeam' AND id=?",
                ("11111111-1111-4111-8111-111111111111",),
            ).fetchone()
        self.assertEqual(tombstone, (1,))


if __name__ == "__main__":
    unittest.main()
