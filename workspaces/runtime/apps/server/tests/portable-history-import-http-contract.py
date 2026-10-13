#!/usr/bin/env python3
"""HTTP boundary contract for portable-history native thread import."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
import sqlite3
import sys
import unittest
from pathlib import Path


isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class PortableHistoryImportHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls, "portable-history-import.json")

    def _agent_count(self) -> int:
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            return database.execute("SELECT COUNT(*) FROM runtime_agents").fetchone()[0]

    def _common_headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_list_import_threads_or_returns_named_rust_refusal(self) -> None:
        before = self._agent_count()
        response = self.backend.request(
            "GET", "/api/import?account_key=default", headers=self._common_headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                "Not implemented: requires provider-session/thread-import",
            )
            self.assertEqual(self._agent_count(), before)
        else:
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(
                response.json()["data"],
                [{"id": "source-thread", "title": "Fixture thread"}],
            )

    def test_import_thread_or_returns_named_rust_refusal_without_writes(self) -> None:
        before = self._agent_count()
        response = self.backend.request(
            "POST",
            "/api/import",
            body={"threadId": "source-thread", "prompt": "Continue the synthetic fixture."},
            headers=self._common_headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                "Not implemented: requires provider-session/thread-import",
            )
            self.assertEqual(self._agent_count(), before)
        else:
            self.assertEqual(response.status, 200, response.body[:1000])
            agent_id = response.json()["id"]
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
                record = database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
                ).fetchone()
            self.assertIsNotNone(record)
            self.assertEqual(json.loads(record[0])["importedFrom"], "source-thread")


if __name__ == "__main__":
    unittest.main()
