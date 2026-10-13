#!/usr/bin/env python3
"""Local system read behavior through the Python and Rust HTTP backends."""

from __future__ import annotations

import os
import sqlite3
import sys
import tempfile
import unittest
import importlib.util
from pathlib import Path
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parent))
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

    runtime = QuietRuntime(backend.state, fixture.FakeServer)
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
        if self.rust_backend:
            requested_paths.append((f"~{user}/{self.home_root.name}/home-child", self.home_root / "home-child"))
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
            for field in ("processTree", "hostResources", "resourceAttribution", "nativeAccounts",
                          "studioLoadedThreads", "queues", "runtimeLockSamples", "sqliteContention",
                          "httpRequestMetrics", "analyticsCapture", "supervisor"):
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


if __name__ == "__main__":
    unittest.main()
