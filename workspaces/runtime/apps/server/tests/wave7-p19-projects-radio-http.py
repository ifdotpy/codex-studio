#!/usr/bin/env python3
"""Local P19 HTTP caller contracts on disposable backends and ephemeral ports."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
import json
import os
import sqlite3
import tempfile
import unittest
from contextlib import closing
from http_backend_harness import BackendProcess, TimedHTTPContractTest, no_write_snapshot
from runtime_test_environment import make_runtime


class Backend(BackendProcess):
    def _environment(self):
        env = super()._environment()
        env.update(TMPDIR=os.environ["TMPDIR"], TMP=os.environ["TMPDIR"], TEMP=os.environ["TMPDIR"])
        return env


class HostProjectsRadio(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        home_root = Path(os.environ["HOME"]).parent / "http"
        home_root.mkdir(exist_ok=True)
        cls.scratch = tempfile.TemporaryDirectory(prefix="cs-w7-p19-http-", dir=os.environ["TMPDIR"])
        cls.addClassCleanup(cls.scratch.cleanup)
        cls.backend = Backend(home_root)
        cls.backend.state = Path(cls.scratch.name) / "state"
        cls.backend.state.mkdir()
        cls.addClassCleanup(cls.backend.close)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        cls.backend.start()
        cls.backend.stop()
        from codex_runtime import Runtime
        runtime = make_runtime(Runtime, cls.backend)
        try:
            with runtime.lock, runtime.db() as db:
                project = runtime.ensure_project(str(cls.backend.cwd), "default", db)
                for key in ("a", "b", "c", "d"):
                    runtime.put(db, "agents", dict(id=key, rootId=key, parentId=None, epoch=0,
                        status="paused", name=key, role="orchestrator", cwd=str(cls.backend.cwd),
                        isLead=True, autoWake=True, approvalPolicy="never", provider="codex",
                        accountKey="default", model="gpt-6-astra", concurrency=32), sync_rooms=False)
                project.update(peerTeams=[dict(id="11111111-1111-4111-8111-111111111111", name="Peers", members=["a", "b"])], peerTeamsRevision=0)
                runtime.put(db, "projects", project)
        finally:
            runtime.close()
        cls.backend.start()

    def headers(self):
        return {"Origin": self.backend.origin, "X-Canvas-Token": self.backend.session["token"], "X-Studio-API-Schema": self.backend.schema_hash}

    def post(self, body):
        return self.backend.request("POST", "/api/peer-teams", body=body, headers=self.headers())

    def test_01_open_stop_replay_restart_and_no_write_denials(self):
        base = dict(action="radio", path=str(self.backend.cwd), team_id="11111111-1111-4111-8111-111111111111")
        body = base | dict(radio_action="open", request_id="p19-http-open")
        opened = self.post(body)
        self.assertEqual(opened.status, 200, opened.body[:1000])
        self.assertEqual(opened.json()["room"]["radio"]["status"], "idle")
        self.assertEqual(opened.json()["room"]["members"], ["a", "b"])
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
                projection = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='room' AND id=?", (opened.json()["room"]["id"],)).fetchone()[0])
            self.assertEqual(projection["value"]["members"], ["a", "b"])
            self.assertEqual(projection["value"]["radio"]["status"], "idle")
        self.backend.stop()
        self.backend.start()
        replay = self.post(body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["room"], opened.json()["room"])
        stop = base | dict(radio_action="stop", request_id="p19-http-stop", expected_revision=0)
        stopped = self.post(stop)
        self.assertEqual(stopped.status, 200, stopped.body[:1000])
        self.assertEqual(stopped.json()["room"]["radio"]["revision"], 1)
        before = no_write_snapshot(self.backend.state)
        conflict = self.post(stop | dict(expected_revision=1))
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        before = no_write_snapshot(self.backend.state)
        send = self.post(base | dict(radio_action="send", request_id="p19-http-send", expected_revision=1, text="message", target="missing"))
        self.assertEqual(send.status, 501 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 400, send.body[:1000])
        if send.status == 501:
            self.assertIn("workspace-state/peer-radio", send.json()["error"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_02_convert_idle_tree_survives_restart(self):
        body = dict(action="convert", path=str(self.backend.cwd), member="a", target="c", expected_revision=0, request_id="p19-http-convert")
        converted = self.post(body)
        self.assertEqual(converted.status, 200, converted.body[:1000])
        self.assertEqual({k: v for k, v in converted.json().items() if k not in {"_syncEntities", "_syncEntitiesAfter"}}, dict(id="a", parentId="c", rootId="c", movedAgents=["a"], peerTeamsRevision=1))
        self.backend.stop()
        self.backend.start()
        replay = self.post(body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual({k:v for k,v in replay.json().items() if k not in {"_syncEntities", "_syncEntitiesAfter"}}, {k:v for k,v in converted.json().items() if k not in {"_syncEntities", "_syncEntitiesAfter"}})
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='a'").fetchone()[0])
            projection = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id='a'").fetchone()[0])
        self.assertFalse(agent["isLead"])
        self.assertFalse(agent["autoWake"])
        self.assertEqual(agent["rootId"], "c")
        self.assertEqual(projection["value"]["rootId"], "c")
        self.assertEqual(projection["value"]["isLead"], False)
        before = no_write_snapshot(self.backend.state)
        changed = self.post(body | dict(target="b"))
        self.assertEqual(changed.status, 400, changed.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_03_host_registration_environment_and_no_write_remote_refusal(self):
        path = self.backend.cwd / "nested"
        path.mkdir(exist_ok=True)
        project = self.backend.request("POST", "/api/projects", body=dict(path=str(path), name="Nested"), headers=self.headers())
        self.assertEqual(project.status, 200, project.body[:1000])
        env = self.backend.request("POST", "/api/projects", body=dict(action="set_worker_environment", path=str(path), environment="host", expected_revision=0), headers=self.headers())
        self.assertEqual(env.status, 200, env.body[:1000])
        self.assertEqual(env.json().get("workerEnvironment", "host"), "host")
        before = no_write_snapshot(self.backend.state)
        remote = self.backend.request("POST", "/api/projects", body=dict(action="register", path=str(path), server="remote", request_id="p19-remote"), headers=self.headers())
        self.assertEqual(remote.status, 501 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 403, remote.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_04_project_account_base_sidebar_and_folder_operations(self):
        path = self.backend.cwd / "operations"
        path.mkdir(exist_ok=True)
        project = self.backend.request("POST", "/api/projects", body=dict(path=str(path), name="Operations"), headers=self.headers())
        self.assertEqual(project.status, 200, project.body[:1000])
        base = self.backend.request("POST", "/api/projects", body=dict(action="set_worker_base", path=str(path), base_ref="main", expected_revision=0), headers=self.headers())
        self.assertEqual(base.status, 200, base.body[:1000])
        accounts = self.backend.request("POST", "/api/projects", body=dict(action="set_accounts", path=str(path), account_keys=["default"], account_key="default", expected_revision=1), headers=self.headers())
        self.assertEqual(accounts.status, 200, accounts.body[:1000])
        folder = self.backend.request("POST", "/api/projects", body=dict(action="add_folder", path=str(path), folder_id="22222222-2222-4222-8222-222222222222", name="Docs", expected_revision=0), headers=self.headers())
        self.assertEqual(folder.status, 200, folder.body[:1000])
        sidebar = self.backend.request("POST", "/api/projects", body=dict(action="reorder", request_id="p19-sidebar", expected_revision=0, groups={"projects": [str(path)]}), headers=self.headers())
        self.assertEqual(sidebar.status, 200, sidebar.body[:1000])
        before = no_write_snapshot(self.backend.state)
        conflict = self.backend.request("POST", "/api/projects", body=dict(action="rename_folder", path=str(path), folder_id="22222222-2222-4222-8222-222222222222", name="Manuals", expected_revision=0), headers=self.headers())
        self.assertEqual(conflict.status, 409 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 400, conflict.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_05_peer_team_save_delete_and_move(self):
        path = str(self.backend.cwd)
        # The earlier conversion advanced the project's peer-team revision.
        saved = self.post(dict(action="save", path=path, request_id="p19-team-save", expected_revision=1,
            team_id="33333333-3333-4333-8333-333333333333", name="Second", members=["c", "d"]))
        self.assertEqual(saved.status, 200, saved.body[:1000])
        moved = self.post(dict(action="move", path=path, request_id="p19-team-move", expected_revision=2,
            team_id="33333333-3333-4333-8333-333333333333", member="b"))
        self.assertEqual(moved.status, 200, moved.body[:1000])
        self.assertEqual(moved.json()["peerTeamsRevision"], 3)
        deleted = self.post(dict(action="delete", path=path, request_id="p19-team-delete", expected_revision=3,
            team_id="33333333-3333-4333-8333-333333333333"))
        self.assertEqual(deleted.status, 200, deleted.body[:1000])
        self.assertEqual(deleted.json()["peerTeamsRevision"], 4)


if __name__ == "__main__":
    unittest.main()
