#!/usr/bin/env python3
"""Terminal HTTP persisted history and local PTY live actions."""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import unittest
import importlib.util
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime

from codex_terminals import TerminalManager
from http_backend_harness import BackendProcess, start_contract_backend

_fixture_spec = importlib.util.spec_from_file_location(
    "terminal_runtime_fixture", Path(__file__).with_name("runtime-contract.py")
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


def terminal_tables(connection: sqlite3.Connection) -> list[tuple[object, ...]]:
    names = (
        "user_terminals",
        "user_terminal_receipts",
        "user_terminal_output",
        "user_terminal_event_cursor",
        "user_terminal_decoder_state",
    )
    return [
        (name, connection.execute(f'SELECT * FROM "{name}" ORDER BY rowid').fetchall())
        for name in names
    ]


class TerminalsHTTPContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.terminal_id = "contract-terminal"
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            original = BackendProcess._bootstrap_python_state

            def bootstrap_with_terminal_tables(backend: BackendProcess, environment: dict[str, str]) -> None:
                original(backend, environment)
                manager = TerminalManager(backend.state)
                cls.seed_python_state(manager, backend)
                manager.close()

            BackendProcess._bootstrap_python_state = bootstrap_with_terminal_tables
            try:
                cls.backend = start_contract_backend(cls)
            finally:
                BackendProcess._bootstrap_python_state = original
        else:
            cls.backend = start_contract_backend(cls)

        cls.manager = None
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            cls.manager = TerminalManager(cls.backend.state)
            cls.seed_python_state(cls.manager, cls.backend)
            cls.addClassCleanup(cls.manager.close)

        # Live terminal creation validates its owner through the Python
        # runtime's normal agent writer before exercising either HTTP backend.
        cls.backend.stop()

        class QuietRuntime(_fixture.Runtime):
            def schedule(self) -> None:
                pass

        runtime = make_runtime(QuietRuntime, cls.backend, _fixture.FakeServer)
        try:
            agent = runtime.create(
                {
                    "name": "Terminal HTTP fixture",
                    "cwd": str(cls.backend.cwd),
                    "prompt": "Seed a harmless terminal contract agent.",
                    "autoWake": False,
                },
                defer=True,
            )
            cls.agent_id = agent["id"]
        finally:
            runtime.close()
        cls.backend.start()

    @classmethod
    def seed_python_state(cls, manager: TerminalManager, backend: BackendProcess) -> None:
        record = {
            "id": cls.terminal_id,
            "agent": "fixture-agent",
            "title": "Archived output",
            "cwd": str(backend.cwd),
            "status": "exited",
            "created": 1.0,
            "updated": 2.0,
            "exitCode": 0,
        }
        with manager.db() as database:
            database.execute(
                "INSERT INTO user_terminals(id,record,output,offset) VALUES (?,?,?,?)",
                (cls.terminal_id, json.dumps(record), "", 0),
            )
        manager.append(cls.terminal_id, "alpha-Ж🙂-omega")

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Canvas-Workspace": self.backend.identity["workspaceId"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_persisted_listing_and_output_read_python_written_state(self) -> None:
        listing = self.backend.request("GET", "/api/terminals", headers=self.headers())
        self.assertEqual(listing.status, 200, listing.body[:1000])
        self.assertEqual(listing.json(), {"items": [{
            "id": self.terminal_id, "agent": "fixture-agent", "title": "Archived output",
            "cwd": str(self.backend.cwd), "status": "exited", "created": 1.0,
            "updated": 2.0, "exitCode": 0,
        }]})

        current = self.backend.request(
            "GET", f"/api/terminals/output?id={self.terminal_id}&offset=6", headers=self.headers(),
        )
        self.assertEqual(current.status, 200, current.body[:1000])
        self.assertEqual(current.json(), {
            "text": "Ж🙂-omega", "offset": len("alpha-Ж🙂-omega"), "truncated": False,
            "status": "exited", "exitCode": 0, "error": None,
        })

        history = self.backend.request(
            "GET", f"/api/terminals/output?id={self.terminal_id}&history=1&offset=0&limit=7",
            headers=self.headers(),
        )
        self.assertEqual(history.status, 200, history.body[:1000])
        self.assertEqual(history.json(), {
            "text": "alpha-Ж", "offset": len("alpha-Ж"),
            "availableOffset": len("alpha-Ж🙂-omega"), "historyStart": 0,
            "hasMore": True, "truncated": False, "status": "exited", "exitCode": 0,
            "error": None,
        })

    def test_invalid_bodies_are_refused_before_terminal_mutation(self) -> None:
        bodies = {
            "/api/terminals/create": {"id": "create", "agent": "fixture-agent", "cols": 1},
            "/api/terminals/input": {"id": self.terminal_id, "text": "x", "request_id": 12},
            "/api/terminals/resize": {"id": self.terminal_id, "cols": 1001},
            "/api/terminals/rename": {"id": self.terminal_id, "title": ""},
            "/api/terminals/close": {},
        }
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            before = terminal_tables(database)
        for path, body in bodies.items():
            response = self.backend.request("POST", path, body=body, headers=self.headers())
            self.assertEqual(response.status, 400, (path, response.body[:1000]))
            with sqlite3.connect(database_path) as database:
                self.assertEqual(terminal_tables(database), before, path)
        for query in ("history=1&limit=0", "history=1&offset=-1"):
            response = self.backend.request(
                "GET", f"/api/terminals/output?id={self.terminal_id}&{query}", headers=self.headers(),
            )
            self.assertEqual(response.status, 400, (query, response.body[:1000]))
            with sqlite3.connect(database_path) as database:
                self.assertEqual(terminal_tables(database), before, query)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_live_operations_create_input_resize_rename_and_close_local_session(self) -> None:
        created = self.backend.request(
            "POST", "/api/terminals/create",
            body={"id": "contract-create", "agent": self.agent_id, "cols": 90, "rows": 25},
            headers=self.headers(),
        )
        self.assertEqual(created.status, 200, created.body[:1000])
        record = created.json()
        self.assertEqual(record["status"], "running")
        terminal_id = record["id"]

        duplicate = self.backend.request(
            "POST", "/api/terminals/create",
            body={"id": "contract-create", "agent": self.agent_id, "cols": 90, "rows": 25},
            headers=self.headers(),
        )
        self.assertEqual(duplicate.status, 200, duplicate.body[:1000])
        self.assertEqual(duplicate.json(), record)

        sent = self.backend.request(
            "POST", "/api/terminals/input",
            body={"id": terminal_id, "text": "printf 'terminal action\\n'\n", "request_id": "contract-input"},
            headers=self.headers(),
        )
        self.assertEqual(sent.status, 200, sent.body[:1000])
        self.assertEqual(sent.json(), {"ok": True, "delivery": "sent"})

        resized = self.backend.request(
            "POST", "/api/terminals/resize",
            body={"id": terminal_id, "cols": 88, "rows": 27},
            headers=self.headers(),
        )
        self.assertEqual(resized.status, 200, resized.body[:1000])

        renamed = self.backend.request(
            "POST", "/api/terminals/rename",
            body={"id": terminal_id, "title": "Contract terminal"},
            headers=self.headers(),
        )
        self.assertEqual(renamed.status, 200, renamed.body[:1000])
        self.assertEqual(renamed.json()["title"], "Contract terminal")

        closed = self.backend.request(
            "POST", "/api/terminals/close", body={"id": terminal_id}, headers=self.headers(),
        )
        self.assertEqual(closed.status, 200, closed.body[:1000])
        self.assertEqual(closed.json()["status"], "closed")


if __name__ == "__main__":
    unittest.main()
