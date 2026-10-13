#!/usr/bin/env python3
"""Local system read behavior through the Python and Rust HTTP backends."""

from __future__ import annotations

import os
import json
from contextlib import closing
import sqlite3
import sys
import tempfile
import unittest
import importlib.util
from pathlib import Path
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def database_snapshot(path: Path):
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as database:
        tables = database.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        ).fetchall()
        return {
            name: database.execute(f'SELECT * FROM "{name}"').fetchall()
            for (name,) in tables
        }


def seed_summary_agent(backend) -> str:
    backend.stop()
    spec = importlib.util.spec_from_file_location(
        "system_reads_runtime_fixture", Path(__file__).with_name("runtime-contract.py")
    )
    fixture = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(fixture)

    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = make_runtime(QuietRuntime, backend, fixture.FakeServer)
    try:
        agent = runtime.create({
            "name": "Summary contract agent", "cwd": str(backend.cwd),
            "prompt": "Seed summary projection.", "autoWake": False,
        }, defer=True)
        agent.update(
            isLead=True, source="managed", status="completed", inFlight=False,
            lastCompletedTurn="summary-turn", lastCompletedTurnStatus="completed",
            threadId="summary-thread", updated=123.0, created=120.0,
            hostProjectPath=str(backend.cwd), cwd=str(backend.cwd),
        )
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
        return agent["id"]
    finally:
        runtime.close()
    return agent["id"]


class SystemReadsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        cls.root = Path(tempfile.mkdtemp(prefix="system-reads-", dir=os.environ.get("TMPDIR")))
        cls.addClassCleanup(__import__("shutil").rmtree, cls.root)
        cls.summary_agent = seed_summary_agent(cls.backend)
        cls.backend.start()
        cls.home_root = Path(tempfile.mkdtemp(prefix="system-reads-home-", dir=Path.home()))
        cls.addClassCleanup(__import__("shutil").rmtree, cls.home_root)
        (cls.home_root / "home-child").mkdir()
        cls.isolated_home_root = cls.backend.home / "expansion"
        cls.isolated_home_root.mkdir()
        (cls.isolated_home_root / "home-child").mkdir()
        for name in ("zulu", "Alpha", ".hidden"):
            (cls.root / name).mkdir()
        (cls.root / "regular-file").write_text("ignored", encoding="utf-8")

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_directory_listing_hides_dot_entries_sorts_and_does_not_write(self):
        before = database_snapshot(self.backend.state / "canvas.sqlite3")
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({"path": str(self.root)}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json(), {
            "path": str(self.root.resolve()),
            "parent": str(self.root.resolve().parent),
            "directories": [
                {"name": "Alpha", "path": str(self.root / "Alpha")},
                {"name": "zulu", "path": str(self.root / "zulu")},
            ],
        })
        self.assertEqual(database_snapshot(self.backend.state / "canvas.sqlite3"), before)

    def test_directory_home_forms_non_strict_resolution_and_symlink(self):
        before = database_snapshot(self.backend.state / "canvas.sqlite3")
        user = __import__("getpass").getuser()
        requested_paths = [
            ("~//expansion/home-child", self.isolated_home_root / "home-child"),
            (str(self.root / "missing" / ".."), self.root),
        ]
        for requested, expected in requested_paths:
            with self.subTest(path=requested):
                response = self.backend.request(
                    "GET", "/api/directories?" + urlencode({"path": requested}),
                    headers=self.headers(),
                )
                self.assertEqual(response.status, 200, response.body[:1000])
                self.assertEqual(response.json()["path"], str(expected.resolve()))
        alias = self.root / "alias"
        alias.symlink_to(self.root / "Alpha", target_is_directory=True)
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({"path": str(alias)}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["path"], str((self.root / "Alpha").resolve()))
        alias.unlink()
        self.assertEqual(database_snapshot(self.backend.state / "canvas.sqlite3"), before)

    def test_directory_listing_is_capped_at_five_hundred(self):
        many = self.root / "many"
        many.mkdir()
        for index in range(505):
            (many / f"entry-{index:03}").mkdir()
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({"path": str(many)}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(len(response.json()["directories"]), 500)

    def test_unavailable_path_is_rejected(self):
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({"path": str(self.root / "missing")}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])

    def test_ui_summary_reads_python_written_projection_without_writes(self):
        before = database_snapshot(self.backend.state / "canvas.sqlite3")
        response = self.backend.request("GET", "/api/ui-summary", headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        summary = response.json()
        self.assertTrue(summary["ready"])
        self.assertTrue(summary["system"])
        self.assertIn(self.summary_agent, {item["id"] for item in summary["chats"]})
        self.assertTrue(any(item["id"] == f"completed:{self.summary_agent}:summary-turn"
                            for item in summary["alerts"]))
        self.assertIsInstance(summary["accounts"], list)
        self.assertEqual(database_snapshot(self.backend.state / "canvas.sqlite3"), before)

    def test_diagnostics_names_the_required_live_schema_fields(self):
        response = self.backend.request("GET", "/api/diagnostics", headers=self.headers())
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            for field in ("process-tree", "host-resources", "native-accounts", "sqlite-contention",
                          "request-metrics", "supervisor-status"):
                self.assertIn(field, response.json()["error"])
        else:
            self.assertEqual(response.status, 200, response.body[:1000])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_remote_directory_browse_names_its_missing_driver(self):
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({"server": "remote-server"}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 403, response.body[:1000])

    def test_empty_and_local_server_ids_use_local_browse(self):
        for server in ("", "local"):
            response = self.backend.request(
                "GET", "/api/directories?" + urlencode({"path": str(self.root), "server": server}),
                headers=self.headers(),
            )
            self.assertEqual(response.status, 200, response.body[:1000])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_saved_local_server_id_uses_local_browse(self):
        self.backend.stop()
        identity_dir = self.backend.state / "multi-server"
        identity_dir.mkdir(exist_ok=True)
        (identity_dir / "identity.json").write_text(
            '{"serverId":"local-server-contract"}', encoding="utf-8"
        )
        self.backend.start()
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({
                "path": str(self.root), "server": "local-server-contract",
            }),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])

    def test_summary_matches_full_python_projection_for_shared_unread_alerts_and_project_order(self):
        from codex_sync_entities import put
        from studio_api.system.ui_summary import read_summary
        path=self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(path) as db:
            original=db.execute("SELECT collection,id,seq,payload,deleted,hash FROM sync_entities").fetchall()
            base=json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",(self.summary_agent,)).fetchone()[0])["value"]
            project={"id":"a-project","path":"/z-project","name":"Project Z"}
            put(db,"project",project["id"],project)
            put(db,"project","z-project",{"id":"z-project","path":"/a-project","name":"Project A"})
            for identity,patch in (("shared-lead",{"sharedRoomId":"shared","readState":{"threadId":"summary-thread","turnId":"summary-turn","read":True,"revision":1},"tail":""}),("remote-lead",{"remoteAnchor":{"home":"remote","link":"link"}}),("fallback-lead",{"hostProjectPath":None,"cwd":"/fallback","name":""})):
                agent={**base,"id":identity,**patch}
                put(db,"agent",identity,agent)
            put(db,"room","shared",{"id":"shared","kind":"private","members":["shared-lead"],"name":"","projectPath":"/shared","radio":{"teamId":"team","revision":1,"status":"idle","speaker":None,"next":[],"active":None,"error":None}})
            put(db,"request","remote-request",{"id":"remote-request","agent":"remote-lead","status":"pending","params":{"questions":[{"id":"q","header":"Header","question":"","options":[]}]}})
            put(db,"complaint","remote-complaint",{"id":"remote-complaint","leadId":"remote-lead","needsUserResponse":True,"title":"","version":1})
        before=database_snapshot(path)
        expected=read_summary(lambda:closing(sqlite3.connect(path))).model_dump(mode="json",by_alias=True,exclude_unset=True)
        response=self.backend.request("GET","/api/ui-summary",headers=self.headers())
        self.assertEqual(response.status,200,response.body[:1000])
        actual=response.json();actual.pop("accounts");expected.pop("accounts")
        self.assertEqual(actual,expected)
        self.assertEqual(database_snapshot(path),before)
        with sqlite3.connect(path) as db:
            db.execute("DELETE FROM sync_entities")
            db.executemany("INSERT INTO sync_entities(collection,id,seq,payload,deleted,hash) VALUES(?,?,?,?,?,?)",original)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"),"strict stored-state refusal")
    def test_corrupt_summary_entity_is_named_and_not_silently_skipped(self):
        path=self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(path) as db:
            original=db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",(self.summary_agent,)).fetchone()[0]
        try:
            for value in ([], {**json.loads(original),"value":{**json.loads(original)["value"],"version":-1}}, {**json.loads(original),"value":{**json.loads(original)["value"],"version":9007199254740992}}):
                with sqlite3.connect(path) as db:
                    db.execute("UPDATE sync_entities SET payload=? WHERE collection='agent' AND id=?",(json.dumps(value),self.summary_agent))
                before=database_snapshot(path)
                response=self.backend.request("GET","/api/ui-summary",headers=self.headers())
                self.assertEqual(response.status,500,response.body)
                self.assertIn("sync_entities/"+self.summary_agent,response.json()["error"])
                self.assertEqual(database_snapshot(path),before)
        finally:
            with sqlite3.connect(path) as db:db.execute("UPDATE sync_entities SET payload=? WHERE collection='agent' AND id=?",(original,self.summary_agent))

    @unittest.skipUnless(hasattr(os,"geteuid") and os.geteuid()!=0,"requires ordinary Unix permissions")
    def test_permission_denied_path_is_redacted(self):
        directory=self.root / "private-permission-marker"
        directory.mkdir()
        directory.chmod(0)
        try:
            response=self.backend.request("GET","/api/directories?"+urlencode({"path":str(directory)}),headers=self.headers())
            self.assertEqual(response.status,400,response.body)
            if self.rust_backend:
                self.assertNotIn(str(directory).encode(),response.body)
                self.assertNotIn(str(directory),self.backend.stderr)
        finally:
            directory.chmod(0o700)
            directory.rmdir()

    def test_dangling_symlink_parent_resolution_and_path_redaction(self):
        link=self.root / "dangling"
        link.symlink_to(self.root / "missing-target")
        try:
            response=self.backend.request("GET","/api/directories?"+urlencode({"path":str(link / ".." / "Alpha")}),headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            self.assertEqual(response.json()["path"],str(self.root / "Alpha"))
            secret_path=str(self.root / "private-unavailable-name")
            response=self.backend.request("GET","/api/directories?"+urlencode({"path":secret_path}),headers=self.headers())
            self.assertEqual(response.status,400,response.body)
            if self.rust_backend:
                self.assertNotIn(secret_path.encode(),response.body)
                self.assertNotIn(secret_path,self.backend.stderr)
        finally:link.unlink()

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"),"named remote dependencies")
    def test_paired_revoked_and_discovered_directory_selection(self):
        path=self.backend.state / "canvas.sqlite3"
        for table,identity,record,expected in (("runtime_access_clients","paired",{"kind":"server","status":"paired"},501),("runtime_access_clients","revoked",{"kind":"server","status":"revoked","revoked":1.0},403),("runtime_access_discovered","discovered",{"status":"discovered"},501)):
            with sqlite3.connect(path) as db:db.execute(f"INSERT INTO {table}(id,record) VALUES(?,?)",(identity,json.dumps({"id":identity,**record})))
            response=self.backend.request("GET","/api/directories?"+urlencode({"server":identity,"path":str(self.root)}),headers=self.headers())
            self.assertEqual(response.status,expected,response.body)
            if identity=="discovered":self.assertIn("discovery-pairing",response.json()["error"])


if __name__ == "__main__":
    unittest.main()
