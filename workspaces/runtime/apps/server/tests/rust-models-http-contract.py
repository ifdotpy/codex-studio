#!/usr/bin/env python3
"""Read Python-written account state and model catalogs through both HTTP backends."""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import BackendProcess, HttpResult


def assert_status(response: HttpResult, expected: int) -> None:
    assert response.status == expected, (expected, response.status, response.body[:2000])


class ModelsHttpContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
        cls.backend = BackendProcess(Path(scratch))
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        cls.backend.hold_provider("model/list")
        cls.backend.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.close()

    def test_reads_catalog_for_python_written_registry_account(self) -> None:
        backend = self.backend
        registry_path = backend.state / "accounts" / "registry.json"
        original_registry = registry_path.read_bytes()
        pending = backend.request("GET", "/api/models")
        assert_status(pending, 400)
        self.assertEqual(pending.json(), {
            "error": (
                "Model catalog is pending; no workers were created. "
                "The existing metadata request remains available for recovery"
            ),
            "catalogPending": True,
        })
        backend.wait_until(lambda: backend.provider_gate_started(), "model/list pending gate")

        starts_before_replay = backend.provider_start_times()
        repeated = backend.request("GET", "/api/models")
        assert_status(repeated, 400)
        self.assertEqual(repeated.json(), pending.json())
        self.assertEqual(backend.provider_start_times(), starts_before_replay)

        backend.release_provider_gate()

        def completed_catalog() -> HttpResult | None:
            response = backend.request("GET", "/api/models")
            return response if response.status == 200 else None

        result = backend.wait_until(
            completed_catalog,
            "model catalog completion",
        )
        assert_status(result, 200)
        body = result.json()
        self.assertEqual(body["data"][0]["model"], "fixture-model")
        self.assertIsNone(body["nextCursor"])
        self.assertEqual(registry_path.read_bytes(), original_registry)

        workers = backend.request("GET", "/api/models?workers=1")
        assert_status(workers, 200)
        worker_body = workers.json()
        self.assertEqual(worker_body["data"][0]["model"], "fixture-model")
        self.assertEqual(worker_body["data"][0]["accountKey"], "default")
        self.assertEqual(worker_body["data"][0]["provider"], "codex")
        self.assertEqual(worker_body["unavailableAccounts"], [])

        frames = backend.received_provider_frames()
        catalog_requests = [frame for frame in frames if frame.get("method") == "model/list"]
        self.assertTrue(catalog_requests)
        self.assertTrue(all(frame["params"]["includeHidden"] is True for frame in catalog_requests))
        self.assertTrue(all(frame["params"]["limit"] == 100 for frame in catalog_requests))
        starts_after_catalog = backend.provider_start_times()
        cached = backend.request("GET", "/api/models")
        assert_status(cached, 200)
        self.assertEqual(cached.json(), body)
        self.assertEqual(backend.provider_start_times(), starts_after_catalog)

    def test_refusal_does_not_start_a_provider_or_change_registry(self) -> None:
        backend = self.backend
        registry_path = backend.state / "accounts" / "registry.json"
        registry_before = registry_path.read_bytes()
        starts_before = backend.provider_start_times()
        invalid = backend.request("GET", "/api/models?account_key=default&account_key=other")
        assert_status(invalid, 400)
        self.assertEqual(invalid.json(), {
            "error": "Invalid request",
            "location": ["query", "account_key"],
        })
        unknown = backend.request("GET", "/api/models?account_key=not-registered")
        assert_status(unknown, 400)
        self.assertEqual(unknown.json(), {"error": "Unknown Codex account"})
        self.assertEqual(backend.provider_start_times(), starts_before)
        self.assertEqual(registry_path.read_bytes(), registry_before)


if __name__ == "__main__":
    unittest.main()
