#!/usr/bin/env python3
"""Tool request endpoint contracts and explicit Rust capability gaps."""

from __future__ import annotations

import os
import sqlite3
import sys
import importlib.util
import unittest
from pathlib import Path
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parent))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def database_snapshot(state: Path):
    with sqlite3.connect(f"file:{state / 'canvas.sqlite3'}?mode=ro", uri=True) as database:
        tables = database.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        ).fetchall()
        return {
            name: tuple(sorted(database.execute(f'SELECT * FROM "{name}"').fetchall(), key=repr))
            for (name,) in tables
        }


def write_receipts(backend, records=(), aliases=()):
    backend.stop()
    spec = importlib.util.spec_from_file_location(
        "tool_requests_runtime_fixture", Path(__file__).with_name("runtime-contract.py")
    )
    fixture = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(fixture)

    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, fixture.FakeServer)
    try:
        with runtime.db() as database:
            for record in records:
                runtime.put(database, "tool_requests", record)
            for alias in aliases:
                database.execute(
                    "INSERT INTO runtime_tool_request_aliases(agent,alias,request) VALUES(?,?,?)",
                    alias,
                )
    finally:
        runtime.close()
    backend.start()


def seed_agents(backend):
    backend.stop()
    spec = importlib.util.spec_from_file_location(
        "tool_requests_runtime_fixture", Path(__file__).with_name("runtime-contract.py")
    )
    fixture = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(fixture)

    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, fixture.FakeServer)
    result = []
    try:
        for name in ("Tool request actor", "Deleted tool request actor"):
            agent = runtime.create({
                "name": name, "cwd": str(backend.cwd), "prompt": "Tool request HTTP fixture.",
                "autoWake": False,
            }, defer=True)
            agent.update(status="idle", autoWake=False)
            with runtime.db() as database:
                runtime.put(database, "agents", agent)
            result.append(agent["id"])
        deleted = runtime.agent(result[1])
        deleted["deletedAt"] = 123.0
        with runtime.db() as database:
            runtime.put(database, "agents", deleted)
    finally:
        runtime.close()
    backend.start()
    return result


def receipt(request_id, actor, *, stage="completed", outcome="applied", **extra):
    return {
        "id": request_id, "agent": actor, "stage": stage, "outcome": outcome,
        "tool": "orchestration_peers", "updated": 123.0, "signature": "secret-signature",
        "result": {"success": True, "contentItems": [{"type": "inputText", "text": "result"}]},
        **extra,
    }


class ToolRequestsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        cls.lead_id, cls.deleted_id = seed_agents(cls.backend)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    @classmethod
    def headers(cls):
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def test_terminal_metadata_list_reads_python_state_and_strips_payload_fields(self):
        write_receipts(self.backend, [receipt("terminal-receipt", self.lead_id)])
        before = database_snapshot(self.backend.state)
        response = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        record = next(row for row in response.json()["requests"] if row["id"] == "terminal-receipt")
        self.assertNotIn("result", record)
        self.assertNotIn("signature", record)
        self.assertNotIn("_payloadBlobs", record)
        self.assertEqual(database_snapshot(self.backend.state), before)

    def test_cancel_missing_is_unknown_and_read_only(self):
        before = database_snapshot(self.backend.state)
        response = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "missing-request"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["stage"], "not_found")
        self.assertEqual(response.json()["outcome"], "unknown")
        self.assertEqual(database_snapshot(self.backend.state), before)

    def test_get_terminal_receipt_and_deleted_actor_refusal(self):
        write_receipts(self.backend, [receipt("terminal-get", self.lead_id)])
        response = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id, "request_id": "terminal-get"}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["id"], "terminal-get")
        self.assertNotIn("signature", response.json())
        deleted = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.deleted_id}),
            headers=self.headers(),
        )
        self.assertEqual(deleted.status, 400, deleted.body[:1000])

    def test_payload_backed_pending_receipt_is_a_narrow_capability_gap_without_writes(self):
        write_receipts(self.backend, [receipt(
            "pending-payload", self.lead_id, stage="queued", outcome="pending",
            result={"success": True, "contentItems": [
                {"type": "inputText", "text": "payload-" * 20_000},
            ]},
        )])
        before = database_snapshot(self.backend.state)
        response = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id}),
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn("payload-store/tool-request-list", response.json()["error"])
        else:
            self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(database_snapshot(self.backend.state), before)

    def test_cancel_queued_receipt_is_durable_replayed_and_preserves_other_tables(self):
        write_receipts(self.backend, [receipt(
            "queued-cancel", self.lead_id, stage="queued", outcome="pending",
            cancelRequested=False, accountKey="default", threadId="thread-a", epoch=1,
        )])
        before = database_snapshot(self.backend.state)
        first = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "queued-cancel"},
            headers=self.headers(),
        )
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(first.json()["stage"], "cancelled")
        self.assertEqual(first.json()["outcome"], "not_applied")
        after_first = database_snapshot(self.backend.state)
        for table, rows in before.items():
            if table != "runtime_tool_requests":
                self.assertEqual(after_first[table], rows, table)
        second = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "queued-cancel"},
            headers=self.headers(),
        )
        self.assertEqual(second.status, 200, second.body[:1000])
        self.assertEqual(second.json()["stage"], "cancelled")
        self.assertEqual(database_snapshot(self.backend.state), after_first)

    def test_cancel_running_refuses_and_foreign_actor_receipt_is_not_exposed(self):
        write_receipts(self.backend, [
            receipt("running-receipt", self.lead_id, stage="running", outcome="unknown"),
            receipt("foreign-receipt", self.lead_id, stage="completed", outcome="applied"),
        ])
        before = database_snapshot(self.backend.state)
        running = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "running-receipt"},
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(running.status, 501, running.body[:1000])
            self.assertIn("tool-driver/request-lifecycle", running.json()["error"])
        else:
            self.assertEqual(running.status, 200, running.body[:1000])
        hidden = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.deleted_id, "request_id": "foreign-receipt"}),
            headers=self.headers(),
        )
        self.assertEqual(hidden.status, 400, hidden.body[:1000])
        if self.rust_backend:
            self.assertEqual(database_snapshot(self.backend.state), before)

    def test_alias_ambiguity_is_preserved_for_get_and_cancel(self):
        write_receipts(
            self.backend,
            [receipt("alias-one", self.lead_id), receipt("alias-two", self.lead_id)],
            [(self.lead_id, "short-alias", "alias-one"), (self.lead_id, "short-alias", "alias-two")],
        )
        before = database_snapshot(self.backend.state)
        for method, path, body in (
            ("GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id, "request_id": "short-alias"}), None),
            ("POST", "/api/tool-requests/cancel", {"agent": self.lead_id, "request_id": "short-alias"}),
        ):
            response = self.backend.request(method, path, body=body, headers=self.headers())
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.json()["stage"], "ambiguous")
            self.assertEqual(response.json()["outcome"], "unknown")
        self.assertEqual(database_snapshot(self.backend.state), before)

    def test_invalid_cancel_body_is_rejected_before_dependency(self):
        response = self.backend.request(
            "POST", "/api/tool-requests/cancel", body={"agent": self.lead_id},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])


if __name__ == "__main__":
    unittest.main()
