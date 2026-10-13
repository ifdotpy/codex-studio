#!/usr/bin/env python3
"""Persisted agent rename behavior against Python and Rust HTTP backends."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import hashlib
import importlib.util
import json
import os
import sqlite3
import sys
import unittest
from pathlib import Path


isolate_supervisor_environment()
from runtime_test_environment import make_runtime

HERE = Path(__file__).resolve().parent
SERVER_SOURCE = HERE.parent / "src"
sys.path.insert(0, str(SERVER_SOURCE))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend

_fixture_spec = importlib.util.spec_from_file_location(
    "agents_create_runtime_fixture", HERE / "runtime-contract.py"
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


def seed_python_agent(backend) -> str:
    """Use Python's production writers to create a stored agent and projection."""
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = make_runtime(QuietRuntime, backend, _fixture.FakeServer)
    try:
        agent = runtime.create(
            {
                "name": "Python-written rename fixture",
                "cwd": str(backend.cwd),
                "prompt": "Read the existing agent before applying the title.",
                "autoWake": False,
            },
            defer=True,
        )
        return agent["id"]
    finally:
        runtime.close()


def rename_state(state: Path, agent_id: str, request_id: str):
    with sqlite3.connect(state / "canvas.sqlite3") as database:
        return {
            "agent": database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,)
            ).fetchone(),
            "projection": database.execute(
                "SELECT seq,hash,payload,deleted FROM sync_entities "
                "WHERE collection='agent' AND id=?",
                (agent_id,),
            ).fetchone(),
            "receipt": database.execute(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?",
                (request_id,),
            ).fetchone(),
        }


class AgentsCreateHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_python_agent(cls.backend)
        cls.backend.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_rename_reads_python_written_agent_and_projection(self):
        before = rename_state(self.backend.state, self.agent_id, "title-request-1")
        self.assertIsNotNone(before["agent"])
        self.assertIsNotNone(before["projection"])
        request = {
            "id": self.agent_id,
            "name": "  Café review  ",
            "request_id": "title-request-1",
        }
        response = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        response_body = response.json()
        self.assertEqual(
            {key: response_body[key] for key in ("id", "name", "request_id", "status")},
            {
                "id": self.agent_id,
                "name": "Café review",
                "request_id": "title-request-1",
                "status": "applied",
            },
        )
        after = rename_state(self.backend.state, self.agent_id, "title-request-1")
        self.assertEqual(
            response_body["_syncEntitiesAfter"], before["projection"][0]
        )
        self.assertEqual(len(response_body["_syncEntities"]), 1)
        entity = response_body["_syncEntities"][0]
        self.assertEqual(entity["id"], f"entity:agent:{self.agent_id}")
        self.assertEqual(entity["seq"], after["projection"][0])
        self.assertFalse(entity["_deleted"])
        self.assertEqual(
            json.loads(entity["payload"])["value"]["name"], "Café review"
        )
        agent = json.loads(after["agent"][0])
        self.assertEqual(agent["name"], "Café review")
        self.assertTrue(agent["manualName"])
        self.assertFalse(agent["needsTitle"])
        projection = json.loads(after["projection"][2])
        self.assertEqual(projection["value"]["name"], "Café review")
        self.assertTrue(projection["value"]["manualName"])
        signature, receipt_result = after["receipt"]
        body = {"operation": "rename", "chat": self.agent_id, "name": "Café review"}
        expected_signature = hashlib.sha256(
            json.dumps(body, sort_keys=True).encode()
        ).hexdigest()
        self.assertEqual(signature, expected_signature)
        self.assertEqual(
            json.loads(receipt_result),
            {
                "id": self.agent_id,
                "name": "Café review",
                "request_id": "title-request-1",
                "status": "applied",
            },
        )

        replay = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(
            replay.json(),
            {
                "id": self.agent_id,
                "name": "Café review",
                "request_id": "title-request-1",
                "status": "applied",
            },
        )
        self.assertNotIn("_syncEntities", replay.json())
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "title-request-1"),
            after,
        )

    def test_rename_conflict_refuses_without_writing(self):
        request = {
            "id": self.agent_id,
            "name": "Original title",
            "request_id": "title-request-conflict",
        }
        first = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(first.status, 200, first.body[:1000])
        before = rename_state(
            self.backend.state, self.agent_id, "title-request-conflict"
        )
        request["name"] = "Changed title"
        conflict = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(
            conflict.json(), {"error": "This request id has different content"}
        )
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "title-request-conflict"),
            before,
        )

    def test_missing_smart_title_request_refuses_without_writing(self):
        before = rename_state(self.backend.state, self.agent_id, "unused-title-id")
        response = self.backend.request(
            "POST",
            "/api/rename",
            body={"id": self.agent_id, "name": None},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(
            response.json(),
            {"error": "Invalid request", "location": ["body"]},
        )
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "unused-title-id"), before
        )

    def test_blank_manual_title_refuses_without_writing(self):
        before = rename_state(self.backend.state, self.agent_id, "unused-title-id")
        response = self.backend.request(
            "POST",
            "/api/rename",
            body={"id": self.agent_id, "name": " "},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(
            response.json(), {"error": "A name must have 1 to 80 characters"}
        )
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "unused-title-id"), before
        )

    def test_smart_title_reports_named_501_without_writing(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("the Python reference backend implements live title generation")
        before = rename_state(self.backend.state, self.agent_id, "smart-title-id")
        response = self.backend.request(
            "POST",
            "/api/rename",
            body={"id": self.agent_id, "name": None, "request_id": "smart-title-id"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 501, response.body[:1000])
        self.assertEqual(
            response.json(),
            {"error": "Not implemented: requires provider-session/title-generation"},
        )
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "smart-title-id"), before
        )


if __name__ == "__main__":
    unittest.main()
