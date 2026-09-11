#!/usr/bin/env python3
"""Project registry and lead directory contracts. No user state or model calls."""

from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request
import uuid

spec = importlib.util.spec_from_file_location(
    "account_fixture", Path(__file__).with_name("runtime-accounts-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_canvas import Canvas, make_server


class ProjectContracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="codex-project-contract-")
        self.root = Path(self.tmp.name).resolve()
        self.first, self.second = self.root / "first", self.root / "second"
        self.first.mkdir()
        self.second.mkdir()
        self.env = patch.dict(os.environ, {
            "CODEX_HOME": str(self.root / "home"),
            "CODEX_CANVAS_CWD": str(self.first),
            "CODEX_AGENTS_STATE_DIR": str(self.root / "state"),
            "CODEX_BOARD_STATE_DIR": "",
        })
        self.env.start()
        self.runtime = fixture.ControlledRuntime(self.root / "state", fixture.AccountServer)

    def tearDown(self):
        self.runtime.close()
        self.env.stop()
        self.tmp.cleanup()

    def restart(self):
        self.runtime.close()
        self.runtime = fixture.ControlledRuntime(self.root / "state", fixture.AccountServer)

    def test_registry_canonical_concurrent_idempotence_and_restart(self):
        alias = self.root / "alias"
        alias.symlink_to(self.first, target_is_directory=True)
        with ThreadPoolExecutor(max_workers=4) as pool:
            projects = list(pool.map(lambda _: self.runtime.projects({"path": str(alias), "name": "Work"}), range(8)))
        project = projects[0]
        self.assertTrue(all(p == project for p in projects))
        self.assertEqual(project["id"], str(self.first))
        self.assertEqual(project["path"], str(self.first))
        self.assertEqual(project["name"], "Work")
        self.assertGreater(project["created"], 0)
        self.assertEqual(self.runtime.projects({"path": str(self.first), "name": "Another name"}), project)
        self.assertEqual(self.runtime.snapshot()["projects"], [project])
        self.restart()
        self.assertEqual(self.runtime.projects(), {"items": [project]})

    def test_directory_names_keep_literal_spaces(self):
        directory = self.root / "folder with spaces "
        directory.mkdir()
        project = self.runtime.projects({"path": str(directory)})
        self.assertEqual(project["path"], str(directory))
        lead = self.runtime.new_lead({"cwd": project["path"]})
        self.assertEqual(lead["cwd"], str(directory))

    def edit_project(self, project, action, **fields):
        return self.runtime.projects({'action': action, 'path': project['path'],
                                      'expected_revision': project.get('organizationRevision', 0), **fields})

    def test_rename_changes_only_studio_metadata_and_survives_restart(self):
        marker = self.first / 'keep.txt'
        marker.write_text('Keep this file')
        original = self.runtime.projects({'path': str(self.first)})
        renamed = self.edit_project(original, 'rename', name='Display name')
        for field in ('id', 'path', 'created', 'accountKey', 'accountRevision'):
            self.assertEqual(renamed[field], original[field])
        self.assertEqual(renamed['name'], 'Display name')
        self.assertEqual(marker.read_text(), 'Keep this file')
        self.assertFalse((self.root / 'Display name').exists())
        self.assertEqual(self.edit_project(original, 'rename', name='Display name'), renamed)
        with self.assertRaises(ValueError):
            self.edit_project(original, 'rename', name='Stale name')
        self.restart()
        self.assertEqual(self.runtime.projects()['items'], [renamed])

    def test_nested_folders_retries_and_empty_removal(self):
        project = self.runtime.projects({'path': str(self.first)})
        parent, child = str(uuid.uuid4()), str(uuid.uuid4())
        request = {'action': 'add_folder', 'path': project['path'], 'expected_revision': 0,
                   'folder_id': parent, 'name': 'Reviews', 'parent_id': None}
        project = self.runtime.projects(request)
        self.assertEqual(self.runtime.projects(request), project)
        with self.assertRaises(ValueError):
            self.runtime.projects({**request, 'name': 'Different content'})
        project = self.edit_project(project, 'add_folder', folder_id=child, name='UI', parent_id=parent)
        self.assertFalse((self.first / 'Reviews').exists())
        with self.assertRaises(ValueError):
            self.edit_project(project, 'add_folder', folder_id=str(uuid.uuid4()), name='ui', parent_id=parent)
        with self.assertRaises(ValueError):
            self.edit_project(project, 'remove_folder', folder_id=parent)
        project = self.edit_project(project, 'rename_folder', folder_id=child, name='Interface')
        self.assertEqual(project['folders'][1]['parentId'], parent)
        self.restart()
        self.assertEqual(self.runtime.projects()['items'], [project])
        project = self.edit_project(project, 'remove_folder', folder_id=child)
        project = self.edit_project(project, 'remove_folder', folder_id=parent)
        self.assertEqual(project['folders'], [])

    def test_move_active_chat_preserves_directory_account_and_work(self):
        project = self.runtime.projects({'path': str(self.first)})
        folder = str(uuid.uuid4())
        project = self.edit_project(project, 'add_folder', folder_id=folder, name='Work')
        lead = self.runtime.new_lead({'cwd': str(self.first)})
        with self.runtime.db() as db:
            lead.update(status='running', inFlight=True, turnId='active-turn')
            self.runtime.put(db, 'agents', lead)
        move = {'project_path': str(self.first), 'project_folder': folder, 'expected_folder': None, 'expected_revision': 0}
        changed = self.runtime.chat_organization(lead['id'], move)
        for field in ('cwd', 'accountKey', 'status', 'inFlight', 'turnId'):
            self.assertEqual(changed[field], lead[field])
        self.assertEqual(changed['projectFolder'], folder)
        self.assertEqual(self.runtime.chat_organization(lead['id'], move), changed)
        with self.assertRaises(ValueError):
            self.edit_project(project, 'remove_folder', folder_id=folder)
        with self.assertRaises(ValueError):
            self.runtime.chat_organization(lead['id'], {**move, 'project_path': str(self.second)})
        with self.assertRaises(ValueError):
            self.runtime.chat_organization(lead['id'], {**move, 'project_folder': None})
        moved = self.runtime.chat_organization(lead['id'], {**move, 'project_folder': None, 'expected_folder': folder, 'expected_revision': 1})
        self.assertIsNone(moved['projectFolder'])
        with self.assertRaises(ValueError):
            self.runtime.chat_organization(lead['id'], move)

    def test_new_chat_folder_is_part_of_the_creation_identity(self):
        project = self.runtime.projects({'path': str(self.first)})
        folder = str(uuid.uuid4())
        self.edit_project(project, 'add_folder', folder_id=folder, name='Work')
        request = {'id': str(uuid.uuid4()), 'cwd': str(self.first), 'project_folder': folder, 'reuse_empty': False}
        lead = self.runtime.new_lead(request)
        self.assertEqual(lead['projectFolder'], folder)
        self.assertEqual(self.runtime.new_lead(request)['id'], lead['id'])
        with self.assertRaises(ValueError):
            self.runtime.new_lead({**request, 'project_folder': None})
        with self.assertRaises(ValueError):
            self.runtime.new_lead({**request, 'id': str(uuid.uuid4()), 'cwd': str(self.second)})
        self.restart()
        self.assertEqual(self.runtime.new_lead(request)['projectFolder'], folder)
        moved = self.runtime.set_account(lead['id'], lead['accountKey'], cwd=str(self.second))
        self.assertNotIn('projectFolder', moved)

    def test_registry_rejects_invalid_folders_and_removes_only_registration(self):
        for path in (None, "", 123, str(self.root / "missing")):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.runtime.projects({"path": path})
        file = self.first / "keep.txt"
        file.write_text("keep")
        with self.assertRaises(ValueError):
            self.runtime.projects({"path": str(file)})
        lead = self.runtime.new_lead({"cwd": str(self.first)})
        self.runtime.projects({"path": str(self.first)})
        self.assertTrue(self.runtime.projects({"action": "remove", "path": str(self.first)})["removed"])
        self.assertFalse(self.runtime.projects({"action": "remove", "path": str(self.first)})["removed"])
        self.assertEqual(file.read_text(), "keep")
        self.assertEqual(self.runtime.agent(lead["id"])["cwd"], str(self.first))
        self.assertEqual(self.runtime.projects(), {"items": []})
        self.runtime.projects({"path": str(self.second)})
        self.second.rmdir()
        self.assertTrue(self.runtime.projects({"action": "remove", "path": str(self.second)})["removed"])

    def test_explicit_directory_reuses_empty_chat_and_retry_survives_restart(self):
        lead = self.runtime.new_lead({})
        with self.runtime.db() as db:
            lead.update(effort="high", fastMode=True, workerDefaults={"model": "gpt-5.6-sol", "effort": "high", "fastMode": True})
            self.runtime.put(db, "agents", lead)
        request = {"id": str(uuid.uuid4()), "previous": lead["id"], "cwd": str(self.second)}
        reused = self.runtime.new_lead(request)
        self.assertEqual(reused["id"], lead["id"])
        self.assertEqual(reused["cwd"], str(self.second))
        for field in ("model", "effort", "fastMode", "workerDefaults"):
            self.assertEqual(reused[field], lead[field])
        self.assertEqual(len(self.runtime.snapshot()["agents"]), 1)
        self.restart()
        self.assertEqual(self.runtime.new_lead(request)["id"], lead["id"])
        with self.assertRaisesRegex(ValueError, "different settings"):
            self.runtime.new_lead({**request, "cwd": str(self.first)})

    def test_explicit_new_chat_keeps_requested_identity_with_empty_previous(self):
        previous = self.runtime.new_lead({"cwd": str(self.first)})
        request = {"id": str(uuid.uuid4()), "previous": previous["id"],
                   "cwd": str(self.first), "reuse_empty": False}
        created = self.runtime.new_lead(request)
        self.assertEqual(created["id"], request["id"])
        self.assertNotEqual(created["id"], previous["id"])
        self.assertEqual(self.runtime.new_lead(request)["id"], request["id"])
        with self.assertRaisesRegex(ValueError, "different settings"):
            self.runtime.new_lead({**request, "reuse_empty": True})

    def test_new_chat_directory_retry_identity_and_existing_defaults(self):
        lead = self.runtime.new_lead({})
        self.runtime.send(lead["id"], "A saved user message", str(uuid.uuid4()))
        request = {"id": str(uuid.uuid4()), "previous": lead["id"], "cwd": str(self.second)}
        created = self.runtime.new_lead(request)
        self.assertNotEqual(created["id"], lead["id"])
        self.assertEqual(created["cwd"], str(self.second))
        self.assertEqual(self.runtime.agent(lead["id"])["cwd"], str(self.first))
        self.restart()
        self.assertEqual(self.runtime.new_lead(request)["id"], created["id"])
        alias = self.root / "second-alias"
        alias.symlink_to(self.second, target_is_directory=True)
        self.assertEqual(self.runtime.new_lead({**request, "cwd": str(alias)})["id"], created["id"])
        with self.assertRaisesRegex(ValueError, "different settings"):
            self.runtime.new_lead({**request, "cwd": str(self.first)})
        self.assertEqual(self.runtime.new_lead({"previous": lead["id"]})["cwd"], str(self.first))

    def test_explicit_directory_keeps_identity_and_validates_directory(self):
        lead = self.runtime.new_lead({})
        reused = self.runtime.new_lead({"previous": lead["id"], "cwd": str(self.second)})
        self.assertEqual(reused["id"], lead["id"])
        self.assertEqual(reused["cwd"], str(self.second))
        for cwd in (None, "", 123, str(self.root / "missing")):
            with self.subTest(cwd=cwd), self.assertRaises(ValueError):
                self.runtime.new_lead({"previous": lead["id"], "cwd": cwd})
        self.assertEqual(self.runtime.agent(lead["id"])["cwd"], str(self.second))
        self.assertEqual(self.runtime.new_lead({})["cwd"], str(self.first))

    def test_account_switch_checks_explicit_directory_before_mutation(self):
        other = self.root / "other-home"
        other.mkdir()
        (other / "auth.json").write_text(json.dumps({"tokens": {"account_id": "other", "access_token": "fixture"}}))
        account = self.runtime.accounts.register(str(other))
        lead = self.runtime.new_lead({})
        changed = self.runtime.new_lead({"previous": lead["id"], "account_key": account, "cwd": str(self.first)})
        self.assertEqual(changed["accountKey"], account)
        self.assertEqual(changed["cwd"], str(self.first))
        reused = self.runtime.new_lead({"previous": lead["id"], "account_key": account, "cwd": str(self.second)})
        self.assertEqual(reused["id"], lead["id"])
        self.assertEqual(reused["cwd"], str(self.second))
        self.assertEqual(reused["accountKey"], account)

    def test_http_projects_snapshot_and_lead_contract(self):
        canvas = Canvas(self.runtime.root)
        canvas.runtime = self.runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        headers = {"Content-Type": "application/json", "Origin": base}

        def request(path, body=None):
            req = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(), headers=headers)
            with urllib.request.urlopen(req, timeout=5) as response:
                return json.loads(response.read())

        try:
            headers["X-Canvas-Token"] = request("/api/state")["token"]
            self.assertEqual(request("/api/projects"), {"items": []})
            project = request("/api/projects", {"path": str(self.second)})
            self.assertEqual(request("/api/projects"), {"items": [project]})
            self.assertEqual(request("/api/state")["runtime"]["projects"], [project])
            lead = request("/api/leads", {"id": str(uuid.uuid4()), "cwd": project["path"]})
            self.assertEqual(lead["cwd"], project["path"])
            self.assertTrue(request("/api/projects", {"action": "remove", "path": project["path"]})["removed"])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main(verbosity=2)
