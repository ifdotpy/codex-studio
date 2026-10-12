#!/usr/bin/env python3
"""Terminal HTTP reads and explicit session-dependent operation status."""

from __future__ import annotations

import json
import os
import sqlite3
import unittest

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from codex_terminals import TerminalManager
from http_backend_harness import BackendProcess, start_contract_backend


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
        self.assertEqual([item["id"] for item in listing.json()["items"]], [self.terminal_id])

        current = self.backend.request(
            "GET", f"/api/terminals/output?id={self.terminal_id}&offset=6", headers=self.headers(),
        )
        self.assertEqual(current.status, 200, current.body[:1000])
        self.assertEqual(current.json()["text"], "Ж🙂-omega")
        self.assertEqual(current.json()["offset"], len("alpha-Ж🙂-omega"))

        history = self.backend.request(
            "GET", f"/api/terminals/output?id={self.terminal_id}&history=1&offset=0&limit=7",
            headers=self.headers(),
        )
        self.assertEqual(history.status, 200, history.body[:1000])
        self.assertEqual(history.json()["text"], "alpha-Ж")
        self.assertTrue(history.json()["hasMore"])
        self.assertEqual(history.json()["availableOffset"], len("alpha-Ж🙂-omega"))

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

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_live_operations_name_the_missing_provider_session(self) -> None:
        bodies = {
            "/api/terminals/create": {"id": "create", "agent": "fixture-agent"},
            "/api/terminals/input": {"id": self.terminal_id, "text": "x", "request_id": "input"},
            "/api/terminals/resize": {"id": self.terminal_id},
            "/api/terminals/rename": {"id": self.terminal_id, "title": "renamed"},
            "/api/terminals/close": {"id": self.terminal_id},
        }
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            before = terminal_tables(database)
        for path, body in bodies.items():
            response = self.backend.request("POST", path, body=body, headers=self.headers())
            self.assertEqual(response.status, 501, (path, response.body[:1000]))
            self.assertEqual(response.json()["error"], "Not implemented: requires provider-session/app-server")
            with sqlite3.connect(database_path) as database:
                self.assertEqual(terminal_tables(database), before, path)


if __name__ == "__main__":
    unittest.main()
