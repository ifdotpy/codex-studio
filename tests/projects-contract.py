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

    def test_explicit_directory_denial_never_falls_back_or_changes_empty_chat(self):
        lead = self.runtime.new_lead({})
        self.runtime.accounts.set_project_rules("default", [str(self.first)], 0)
        for previous in (None, lead["id"]):
            request = {"id": str(uuid.uuid4()), "cwd": str(self.second)}
            if previous:
                request["previous"] = previous
            with self.subTest(previous=previous), self.assertRaisesRegex(ValueError, "cannot use project"):
                self.runtime.new_lead(request)
        self.assertEqual(self.runtime.agent(lead["id"])["cwd"], str(self.first))
        self.assertEqual(len(self.runtime.snapshot()["agents"]), 1)
        for cwd in (None, "", 123, str(self.root / "missing")):
            with self.subTest(cwd=cwd), self.assertRaises(ValueError):
                self.runtime.new_lead({"previous": lead["id"], "cwd": cwd})
        self.assertEqual(self.runtime.new_lead({})["cwd"], str(self.first))

    def test_account_switch_checks_explicit_directory_before_mutation(self):
        other = self.root / "other-home"
        other.mkdir()
        (other / "auth.json").write_text(json.dumps({"tokens": {"account_id": "other", "access_token": "fixture"}}))
        account = self.runtime.accounts.register(str(other))
        self.runtime.accounts.set_project_rules(account, [str(self.second)], 0)
        lead = self.runtime.new_lead({})
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.new_lead({"previous": lead["id"], "account_key": account, "cwd": str(self.first)})
        self.assertEqual(self.runtime.agent(lead["id"])["accountKey"], "default")
        reused = self.runtime.new_lead({"previous": lead["id"], "account_key": account, "cwd": str(self.second)})
        self.assertEqual(reused["id"], lead["id"])
        self.assertEqual(reused["cwd"], str(self.second))
        self.assertEqual(reused["accountKey"], account)
        self.assertFalse(reused["dangerouslySkipAccountRules"])

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
