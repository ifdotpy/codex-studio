#!/usr/bin/env python3
"""Terminal HTTP persisted history and local PTY live actions."""

from __future__ import annotations

import json
import http.client
import os
import signal
import sqlite3
import sys
import unittest
import importlib.util
from pathlib import Path
from urllib.parse import urlsplit

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

    def restart_backend(self) -> None:
        self.backend.stop()
        self.backend.start()

    def create_live_terminal(self, request_id: str) -> tuple[str, dict[str, object]]:
        response = self.backend.request(
            "POST", "/api/terminals/create",
            body={"id": request_id, "agent": self.agent_id, "cols": 90, "rows": 25},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        record = response.json()
        return record["id"], record

    def seed_running_terminal(self, terminal_id: str, identity: object) -> None:
        record = {
            "id": terminal_id,
            "agent": "fixture-agent",
            "title": "Restart fixture",
            "cwd": str(self.backend.cwd),
            "status": "running",
            "created": 1.0,
            "updated": 2.0,
            "exitCode": None,
            "_processIdentity": identity,
        }
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute(
                "INSERT INTO user_terminals(id,record,output,offset) VALUES (?,?,?,?)",
                (terminal_id, json.dumps(record), "", 0),
            )

    @staticmethod
    def stale_process_identity() -> dict[str, object]:
        return {
            "pid": 2_000_000_000,
            "process_group": 2_000_000_000,
            "session": 2_000_000_000,
            "start_time_ticks": 1,
            "boot_id": "unavailable-test-boot",
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
        bodies = [
            ("/api/terminals/create", {"id": "create", "agent": "fixture-agent", "cols": 1}),
            ("/api/terminals/create", {"id": "create-bool-size", "agent": "fixture-agent", "cols": True}),
            ("/api/terminals/create", {"id": "create-unknown-owner", "agent": "not-owned-agent", "cols": 100, "rows": 28}),
            ("/api/terminals/input", {"id": self.terminal_id, "text": "x", "request_id": 12}),
            ("/api/terminals/resize", {"id": self.terminal_id, "cols": 1001}),
            ("/api/terminals/rename", {"id": self.terminal_id, "title": ""}),
            ("/api/terminals/close", {}),
        ]
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            before = terminal_tables(database)
        for path, body in bodies:
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

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_restart_cleanup_failure_keeps_server_up_and_retries_before_writes(self) -> None:
        failing_id = "restart-cleanup-failing"
        healthy_id = "restart-cleanup-healthy"
        self.seed_running_terminal(failing_id, {"pid": "malformed"})
        self.seed_running_terminal(healthy_id, self.stale_process_identity())
        self.backend.stop()
        self.backend.start()

        health = self.backend.request("GET", "/api/session", headers=self.headers())
        self.assertEqual(health.status, 200, health.body[:1000])
        listing = self.backend.request("GET", "/api/terminals", headers=self.headers())
        self.assertEqual(listing.status, 200, listing.body[:1000])
        self.assertEqual(
            {item["id"]: item["status"] for item in listing.json()["items"]
             if item["id"] in {failing_id, healthy_id}},
            {failing_id: "running", healthy_id: "running"},
        )
        diagnostic = "terminal restart reconciliation failed (kind=session)"
        self.assertEqual(self.backend.stderr.count(diagnostic), 1)

        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            before = terminal_tables(database)
        writes = [
            ("/api/terminals/create", {"id": "blocked-create", "agent": self.agent_id}),
            ("/api/terminals/input", {"id": failing_id, "text": "x", "request_id": "blocked-input"}),
            ("/api/terminals/resize", {"id": failing_id, "cols": 90, "rows": 25}),
            ("/api/terminals/rename", {"id": failing_id, "title": "blocked"}),
            ("/api/terminals/close", {"id": failing_id}),
        ]
        for path, body in writes:
            response = self.backend.request("POST", path, body=body, headers=self.headers())
            self.assertEqual(response.status, 501, (path, response.body[:1000]))
            self.assertEqual(
                response.json(),
                {"error": "Not implemented: requires terminal-driver/restart-cleanup"},
                path,
            )
            with sqlite3.connect(database_path) as database:
                self.assertEqual(terminal_tables(database), before, path)

        with sqlite3.connect(database_path) as database:
            record = json.loads(database.execute(
                "SELECT record FROM user_terminals WHERE id=?", (failing_id,),
            ).fetchone()[0])
            record["_processIdentity"] = self.stale_process_identity()
            database.execute(
                "UPDATE user_terminals SET record=? WHERE id=?",
                (json.dumps(record), failing_id),
            )
        renamed = self.backend.request(
            "POST", "/api/terminals/rename",
            body={"id": failing_id, "title": "reconciled"}, headers=self.headers(),
        )
        self.assertEqual(renamed.status, 200, renamed.body[:1000])
        self.assertEqual(renamed.json()["title"], "reconciled")
        with sqlite3.connect(database_path) as database:
            for terminal_id in (failing_id, healthy_id):
                record = json.loads(database.execute(
                    "SELECT record FROM user_terminals WHERE id=?", (terminal_id,),
                ).fetchone()[0])
                self.assertEqual(record["status"], "exited")
                self.assertNotIn("_processIdentity", record)
        self.assertEqual(self.backend.stderr.count(diagnostic), 1)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_restart_create_input_and_close_receipts_replay(self) -> None:
        create_body = {"id": "restart-create-receipt", "agent": self.agent_id,
                       "cols": 90, "rows": 25}
        created = self.backend.request(
            "POST", "/api/terminals/create", body=create_body, headers=self.headers(),
        )
        self.assertEqual(created.status, 200, created.body[:1000])
        created_record = created.json()
        self.restart_backend()
        replayed_create = self.backend.request(
            "POST", "/api/terminals/create", body=create_body, headers=self.headers(),
        )
        self.assertEqual(replayed_create.status, 200, replayed_create.body[:1000])
        self.assertEqual(replayed_create.json(), created_record)
        current_create = self.backend.request(
            "GET", f"/api/terminals/output?id={created_record['id']}&offset=0",
            headers=self.headers(),
        )
        self.assertEqual(current_create.status, 200, current_create.body[:1000])
        self.assertEqual(current_create.json()["status"], "exited")
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            current_record = json.loads(database.execute(
                "SELECT record FROM user_terminals WHERE id=?", (created_record["id"],),
            ).fetchone()[0])
        self.assertNotIn("_processIdentity", current_record)

        input_id, _ = self.create_live_terminal("restart-input-terminal")
        marker = "CS_W7_RESTART_INPUT_ONCE"
        input_body = {
            "id": input_id,
            "text": f"printf '{marker}\\n'\n",
            "request_id": "restart-input-receipt",
        }
        endpoint = urlsplit(self.backend.origin)
        connection = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=5)
        connection.request(
            "POST", "/api/terminals/input", body=json.dumps(input_body),
            headers={**self.headers(), "Content-Type": "application/json"},
        )

        database_path = self.backend.state / "canvas.sqlite3"

        def input_settled() -> bool:
            with sqlite3.connect(database_path) as database:
                row = database.execute(
                    "SELECT result FROM user_terminal_receipts WHERE id=?",
                    (input_body["request_id"],),
                ).fetchone()
            return row is not None and json.loads(row[0]) == {"ok": True, "delivery": "sent"}

        self.backend.wait_until(input_settled, "input receipt committed after response loss")
        connection.close()  # The server response is discarded without reading it.

        def marker_visible() -> bool:
            response = self.backend.request(
                "GET", f"/api/terminals/output?id={input_id}&offset=0", headers=self.headers(),
            )
            return response.status == 200 and marker in response.json()["text"]

        self.backend.wait_until(marker_visible, "input reached terminal output")
        before = self.backend.request(
            "GET", f"/api/terminals/output?id={input_id}&offset=0", headers=self.headers(),
        ).json()["text"]
        self.restart_backend()
        replayed_input = self.backend.request(
            "POST", "/api/terminals/input", body=input_body, headers=self.headers(),
        )
        self.assertEqual(replayed_input.status, 200, replayed_input.body[:1000])
        self.assertEqual(replayed_input.json(), {"ok": True, "delivery": "sent"})
        after = self.backend.request(
            "GET", f"/api/terminals/output?id={input_id}&offset=0", headers=self.headers(),
        ).json()["text"]
        self.assertEqual(after.count(marker), before.count(marker))

        close_id, _ = self.create_live_terminal("restart-close-terminal")
        close_body = {"id": close_id}
        closed = self.backend.request(
            "POST", "/api/terminals/close", body=close_body, headers=self.headers(),
        )
        self.assertEqual(closed.status, 200, closed.body[:1000])
        self.assertEqual(closed.json()["status"], "closed")
        self.restart_backend()
        replayed_close = self.backend.request(
            "POST", "/api/terminals/close", body=close_body, headers=self.headers(),
        )
        self.assertEqual(replayed_close.status, 200, replayed_close.body[:1000])
        self.assertEqual(replayed_close.json()["status"], "closed")

        running_id, _ = self.create_live_terminal("restart-running-terminal")
        process = self.backend._process
        self.assertIsNotNone(process)
        process.kill()
        process.wait(timeout=5)
        if self.backend._reader is not None:
            self.backend._reader.join(timeout=5)
        for stream in (process.stdout, process.stderr):
            if stream is not None:
                stream.close()
        self.backend._process = None
        self.backend.origin = None
        self.backend.start()
        recovered = self.backend.request(
            "GET", f"/api/terminals/output?id={running_id}&offset=0", headers=self.headers(),
        )
        self.assertEqual(recovered.status, 200, recovered.body[:1000])
        self.assertEqual(recovered.json()["status"], "exited")
        self.assertEqual(
            recovered.json()["error"],
            "The server restarted. This shell cannot be resumed.",
        )
        with sqlite3.connect(database_path) as database:
            record = json.loads(database.execute(
                "SELECT record FROM user_terminals WHERE id=?", (running_id,),
            ).fetchone()[0])
        self.assertNotIn("_processIdentity", record)


if __name__ == "__main__":
    unittest.main()
