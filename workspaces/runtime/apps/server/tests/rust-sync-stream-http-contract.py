#!/usr/bin/env python3
"""Protocol-3 sync stream handshake and committed-write notification contracts."""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import time
import unittest
import uuid
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

from test_isolation import isolate_supervisor_environment
from sync_http_contract_support import prepare_sync_store_for_rust_contract

isolate_supervisor_environment()

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "src"))
from http_backend_harness import (
    HTTP_WAIT_GUARD_SECONDS,
    TimedHTTPContractTest,
    start_contract_backend,
)

_fixture_spec = importlib.util.spec_from_file_location(
    "sync_stream_runtime_fixture", HERE / "runtime-contract.py"
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


def seed_agent_and_queue(backend) -> tuple[str, list[str]]:
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    try:
        agent = runtime.create(
            {
                "name": "Sync stream contract agent",
                "cwd": str(backend.cwd),
                "prompt": "Seed idle work and queue state for the sync stream contract.",
                "autoWake": False,
            },
            defer=True,
        )
        agent.update(autoWake=False, status="idle")
        queue_ids = [f"sync-stream-queue-{uuid.uuid4().hex[:12]}-{index}" for index in range(2)]
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
            for index, queue_id in enumerate(queue_ids):
                runtime.enqueue(database, agent, "user", f"Queue item {index}", queue_id)
        return agent["id"], queue_ids
    finally:
        runtime.close()
        backend.start()


def read_event(response) -> dict[str, object] | None:
    deadline = time.monotonic() + HTTP_WAIT_GUARD_SECONDS
    raw_socket = response.fp.raw._sock
    fields: dict[str, str] = {}
    data_lines: list[str] = []
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("sync stream did not produce a frame within the HTTP guard")
        raw_socket.settimeout(remaining)
        line = response.readline()
        if not line:
            return None
        text = line.decode("utf-8").rstrip("\r\n")
        if not text:
            if not fields and not data_lines:
                continue
            if data_lines:
                fields["data"] = "\n".join(data_lines)
            event: dict[str, object] = dict(fields)
            if "data" in fields:
                event["data"] = json.loads(fields["data"])
            return event
        if text.startswith(":"):
            continue
        key, _, value = text.partition(":")
        value = value.removeprefix(" ")
        if key == "data":
            data_lines.append(value)
        else:
            fields[key] = value


class SyncStreamHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id, cls.queue_ids = seed_agent_and_queue(cls.backend)
        prepare_sync_store_for_rust_contract(cls.backend, seed_python_rows=True)
        cls.python_entity_sequence = None
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            import sqlite3

            with sqlite3.connect(f"file:{cls.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
                cls.python_entity_sequence = db.execute(
                    "SELECT COALESCE(MAX(seq),0) FROM sync_entities "
                    "WHERE collection NOT LIKE 'transcript:%'"
                ).fetchone()[0]

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def stream_path(self, *, schema: str | None = None) -> str:
        query = {
            "protocol": "3",
            "resources": json.dumps([{"kind": "state"}], separators=(",", ":")),
        }
        if schema is not None:
            query["apiSchema"] = schema
        return "/api/sync/stream?" + urlencode(query)

    def open_stream(self, path: str | None = None, extra_headers=None):
        request_headers = extra_headers or {}
        return urlopen(
            Request(self.backend.origin + (path or self.stream_path(schema=self.backend.schema_hash)),
                    headers=request_headers),
            timeout=HTTP_WAIT_GUARD_SECONDS,
        )

    def next_resource_event(self, response) -> dict[str, object]:
        while True:
            event = read_event(response)
            self.assertIsNotNone(event, "stream closed before a resource event")
            if event["event"] == "resources":
                return event

    def test_initial_frames_reconnect_and_schema_mismatch(self) -> None:
        with self.open_stream() as response:
            first = read_event(response)
            second = read_event(response)
            self.assertEqual([first["event"], second["event"]], ["api-schema", "resources"])
            self.assertEqual(first["data"], {"hash": self.backend.schema_hash})
            initial = second["data"]
            self.assertEqual(initial["protocol"], 3)
            self.assertTrue(initial["workspaceId"])
            self.assertEqual(initial["reason"], "initial")
            self.assertEqual(initial["resources"], [{"kind": "state"}])
            self.assertEqual(len(initial["resourceVersions"]), 1)
            if self.python_entity_sequence is not None:
                self.assertGreater(self.python_entity_sequence, 0)
                self.assertEqual(
                    initial["resourceVersions"][0]["revision"],
                    self.python_entity_sequence,
                )
            event_id = second["id"]
            self.assertTrue(event_id)

        with self.open_stream(extra_headers={"Last-Event-ID": str(event_id)}) as response:
            self.assertEqual(read_event(response)["event"], "api-schema")
            resumed = self.next_resource_event(response)
            self.assertEqual(resumed["data"]["reason"], "reconnect")
            self.assertEqual(resumed["data"]["resources"], [{"kind": "state"}])

        with self.open_stream(self.stream_path(schema="different")) as response:
            mismatch = read_event(response)
            self.assertEqual(mismatch["event"], "api-schema")
            self.assertEqual(mismatch["data"], {
                "hash": self.backend.schema_hash,
                "mismatch": True,
            })
            self.assertIsNone(read_event(response))

        with self.open_stream(
            self.stream_path(schema="different"),
            extra_headers={"X-Studio-API-Schema": self.backend.schema_hash},
        ) as response:
            first = read_event(response)
            second = read_event(response)
            self.assertEqual([first["event"], second["event"]], ["api-schema", "resources"])
            self.assertEqual(first["data"], {"hash": self.backend.schema_hash})

        unsupported = self.backend.request(
            "GET", "/api/sync/stream?protocol=2", headers=self.headers()
        )
        self.assertEqual(unsupported.status, 426, unsupported.body[:1000])

        invalid_resources = self.backend.request(
            "GET",
            "/api/sync/stream?" + urlencode({
                "protocol": "3",
                "apiSchema": self.backend.schema_hash,
                "resources": "[]",
            }),
            headers=self.headers(),
        )
        self.assertEqual(invalid_resources.status, 400, invalid_resources.body[:1000])
        with self.open_stream() as response:
            self.assertEqual(read_event(response)["event"], "api-schema")
            after_refusal = self.next_resource_event(response)
            self.assertEqual(after_refusal["id"], event_id)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_stream_advances_for_every_committed_rust_writer(self) -> None:
        room_id = str(uuid.uuid4())
        with self.open_stream() as response:
            first = read_event(response)
            self.assertEqual(first["event"], "api-schema")
            initial = self.next_resource_event(response)
            revision = int(initial["id"])
            queue_snapshot_response = self.backend.request(
                "GET", f"/api/queue?agent={self.agent_id}", headers=self.headers()
            )
            self.assertEqual(queue_snapshot_response.status, 200, queue_snapshot_response.body[:1000])
            queue_snapshot = queue_snapshot_response.json()
            ordered_queue_ids = [item["id"] for item in queue_snapshot["items"]]
            ordered_queue_ids.reverse()
            writes = [
                ("POST", "/api/chats", {
                    "id": room_id, "name": "Sync stream room", "members": [],
                }),
                ("POST", "/api/connections", {
                    "source": self.agent_id, "target": room_id, "connected": True,
                }),
                ("POST", "/api/messages", {
                    "id": str(uuid.uuid4()), "room": room_id, "text": "Committed message",
                }),
                ("POST", "/api/queue", {
                    "agent": self.agent_id, "action": "reorder",
                    "request_id": "sync-stream-queue-reorder",
                    "expected_revision": queue_snapshot["revision"],
                    "ordered_ids": ordered_queue_ids,
                }),
                ("POST", "/api/work", {
                    "agent": self.agent_id, "action": "create", "id": "sync-stream-work-create",
                    "title": "Stream-visible work item",
                }),
                ("POST", "/api/plan", {
                    "agent": self.agent_id, "id": "sync-stream-plan", "version": 0,
                    "text": "Stream-visible plan",
                }),
                ("POST", "/api/annotation", {
                    "agent": self.agent_id, "id": "sync-stream-annotation", "path": "src/main.rs",
                    "line": 1, "text": "Stream-visible annotation",
                }),
                ("POST", "/api/sync/preferences", {
                    "scope": "user",
                    "fields": {
                        '["codex-studio-preferences-v1","theme"]': {
                            "value": "dark", "timestamp": 1_000_000_000, "writer": "sync-stream",
                        },
                    },
                }),
            ]
            for method, path, body in writes:
                with self.subTest(path=path):
                    response_result = self.backend.request(
                        method, path, body=body, headers=self.headers()
                    )
                    self.assertEqual(response_result.status, 200, response_result.body[:1000])
                    event = self.next_resource_event(response)
                    payload = event["data"]
                    self.assertEqual(payload["reason"], "change", payload)
                    self.assertEqual(int(event["id"]), revision + 1, payload)
                    self.assertEqual(payload["revision"], revision + 1)
                    revision += 1

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_z_shutdown_closes_stream_without_forced_process_kill(self) -> None:
        response = self.open_stream()
        self.addCleanup(response.close)
        self.assertEqual(read_event(response)["event"], "api-schema")
        self.next_resource_event(response)
        self.backend.stop()
        self.assertIsNone(read_event(response), "graceful shutdown ends an active SSE response")
        self.assertEqual(self.backend.stop_phases[-1]["forcedKill"], 0.0)


if __name__ == "__main__":
    unittest.main()
