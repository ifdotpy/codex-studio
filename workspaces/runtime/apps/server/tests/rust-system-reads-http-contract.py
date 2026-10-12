#!/usr/bin/env python3
"""Local system read behavior through the Python and Rust HTTP backends."""

from __future__ import annotations

import os
import sqlite3
import sys
import tempfile
import unittest
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


class SystemReadsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        cls.root = Path(tempfile.mkdtemp(prefix="system-reads-", dir=os.environ.get("TMPDIR")))
        cls.addClassCleanup(__import__("shutil").rmtree, cls.root)
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

    def test_unavailable_path_is_rejected(self):
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({"path": str(self.root / "missing")}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])

    def test_runtime_snapshots_are_named_partial_operations_in_rust(self):
        for path, dependency in (
            ("/api/diagnostics", "runtime-process/diagnostics"),
            ("/api/ui-summary", "system-reads/ui-summary"),
        ):
            response = self.backend.request("GET", path, headers=self.headers())
            if self.rust_backend:
                self.assertEqual(response.status, 501, response.body[:1000])
                self.assertIn(dependency, response.json()["error"])
            else:
                self.assertEqual(response.status, 200, response.body[:1000])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_remote_directory_browse_names_its_missing_driver(self):
        response = self.backend.request(
            "GET", "/api/directories?" + urlencode({"server": "remote-server"}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 501, response.body[:1000])
        self.assertIn("multi-server/folder-browse", response.json()["error"])


if __name__ == "__main__":
    unittest.main()
