#!/usr/bin/env python3
"""HTTP contracts for profiles and the currently partial progress panel routes."""

from __future__ import annotations

import os
import sqlite3
import unittest
import uuid
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def profile_rows(state):
    with sqlite3.connect(f"file:{state / 'canvas.sqlite3'}?mode=ro", uri=True) as database:
        return database.execute(
            "SELECT id,record FROM runtime_profiles ORDER BY rowid"
        ).fetchall()


def profile_table(state):
    with sqlite3.connect(state / "canvas.sqlite3") as database:
        return database.execute(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='runtime_profiles'"
        ).fetchone()


class PanelsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        if cls.rust_backend:
            cls.backend.stop()
            with patch.dict(os.environ, {"STUDIO_HTTP_BACKEND_COMMAND": ""}):
                cls.backend.start()
                response = cls.backend.request(
                    "POST",
                    "/api/profiles",
                    body={
                        "id": "python-profile",
                        "name": "Python profile",
                        "role": "reviewer",
                        "model": "fixture-model",
                        "effort": "medium",
                        "instructions": "Written by Python.",
                    },
                    headers=cls.headers(),
                )
                if response.status != 200:
                    raise AssertionError(f"Python profile seed failed: HTTP {response.status}")
                cls.backend.stop()
            cls.backend.start()
            cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)
        else:
            response = cls.backend.request(
                "POST",
                "/api/profiles",
                body={
                    "id": "python-profile",
                    "name": "Python profile",
                    "role": "reviewer",
                    "model": "fixture-model",
                    "effort": "medium",
                    "instructions": "Written by Python.",
                },
                headers=cls.headers(),
            )
            if response.status != 200:
                raise AssertionError(f"Python profile seed failed: HTTP {response.status}")

    @classmethod
    def headers(cls):
        backend = cls.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def test_profiles_read_python_written_records_and_save_delete(self) -> None:
        backend = self.backend
        common = self.headers()
        response = backend.request("GET", "/api/profiles", headers=common)
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertIn(
            {
                "id": "python-profile",
                "name": "Python profile",
                "role": "reviewer",
                "model": "fixture-model",
                "effort": "medium",
                "instructions": "Written by Python.",
            },
            response.json()["profiles"],
        )

        created = backend.request(
            "POST",
            "/api/profiles",
            body={
                "id": "rust-profile",
                "name": "\u001cRust profile\u001c",
                "role": "implementer",
                "model": "fixture-model",
                "effort": "high",
                "instructions": "  Test instructions  ",
            },
            headers=common,
        )
        self.assertEqual(created.status, 200, created.body[:1000])
        if self.rust_backend:
            self.assertEqual(created.json()["name"], "\u001cRust profile\u001c")
            self.assertEqual(created.json()["instructions"], "Test instructions")
        else:
            self.assertEqual(created.json()["name"], "Rust profile")
        listed = backend.request("GET", "/api/profiles", headers=common)
        self.assertEqual(listed.status, 200, listed.body[:1000])
        self.assertTrue(any(item["id"] == "rust-profile" for item in listed.json()["profiles"]))

        generated = backend.request(
            "POST",
            "/api/profiles",
            body={"name": "Generated ID", "model": "fixture-model"},
            headers=common,
        )
        self.assertEqual(generated.status, 200, generated.body[:1000])
        generated_id = uuid.UUID(generated.json()["id"])
        self.assertEqual(generated_id.version, 4)
        self.assertEqual(generated_id.variant, uuid.RFC_4122)

        deleted = backend.request(
            "POST", "/api/profiles", body={"action": "delete", "id": "rust-profile"},
            headers=common,
        )
        self.assertEqual(deleted.status, 200, deleted.body[:1000])
        self.assertEqual(deleted.json()["deleted"], "rust-profile")
        deleted_generated = backend.request(
            "POST",
            "/api/profiles",
            body={"action": "delete", "id": str(generated_id)},
            headers=common,
        )
        self.assertEqual(deleted_generated.status, 200, deleted_generated.body[:1000])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_partial_panel_refusal_does_not_write_state(self) -> None:
        before = profile_rows(self.backend.state)
        panel = self.backend.request("GET", "/api/panel", headers=self.headers())
        self.assertEqual(panel.status, 501, panel.body[:1000])
        self.assertEqual(
            panel.json()["error"],
            "Not implemented: requires resource-policy/read-progress-panel",
        )
        valid_layout = {
            "agent": "agent-panel-contract",
            "revision": "revision-1",
            "client": "phone",
            "sequence": 1,
            "renderer": "progress-markdown-v1",
            "width": 320,
            "height": 150,
            "contentWidth": 100,
            "contentHeight": 40,
            "fits": True,
            "reason": None,
        }
        refused = self.backend.request(
            "POST", "/api/panel/layout", body=valid_layout, headers=self.headers(),
        )
        self.assertEqual(refused.status, 501, refused.body[:1000])
        self.assertEqual(
            refused.json()["error"],
            "Not implemented: requires resource-policy/write-progress-layout",
        )
        self.assertFalse((self.backend.state / "progress" / "agent-panel-contract").exists())
        self.assertEqual(profile_rows(self.backend.state), before)

        invalid = self.backend.request(
            "POST",
            "/api/panel/layout",
            body={key: value for key, value in valid_layout.items() if key != "revision"},
            headers=self.headers(),
        )
        self.assertEqual(invalid.status, 400, invalid.body[:1000])
        self.assertEqual(invalid.json()["location"], ["body", "revision"])
        self.assertEqual(profile_rows(self.backend.state), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_python_profile_is_refused_without_writes(self) -> None:
        state = self.backend.state
        with sqlite3.connect(state / "canvas.sqlite3") as database:
            original = database.execute(
                "SELECT record FROM runtime_profiles WHERE id='python-profile'"
            ).fetchone()[0]
        invalid_records = [
            ('{"id":"python-profile","name":"Bad","role":"future",'
             '"model":"fixture-model","effort":null,"instructions":""}'),
            ('{"id":"python-profile","name":"Missing effort","role":"reviewer",'
             '"model":"fixture-model","instructions":""}'),
            ('{"id":"python-profile","name":"Unknown field","role":"reviewer",'
             '"model":"fixture-model","effort":null,"instructions":"",'
             '"future":true}'),
            sqlite3.Binary(b'{"id":"python-profile"}'),
        ]
        try:
            for invalid_record in invalid_records:
                with sqlite3.connect(state / "canvas.sqlite3") as database:
                    database.execute(
                        "UPDATE runtime_profiles SET record=? WHERE id='python-profile'",
                        (invalid_record,),
                    )
                before = profile_rows(state)
                refused = self.backend.request("GET", "/api/profiles", headers=self.headers())
                self.assertEqual(refused.status, 500, refused.body[:1000])
                self.assertEqual(
                    refused.json()["error"], "Invalid stored object profiles/python-profile",
                )
                self.assertEqual(profile_rows(state), before)
        finally:
            with sqlite3.connect(state / "canvas.sqlite3") as database:
                database.execute(
                    "UPDATE runtime_profiles SET record=? WHERE id='python-profile'", (original,),
                )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_save_refuses_to_replace_invalid_profile_without_writes(self) -> None:
        state = self.backend.state
        with sqlite3.connect(state / "canvas.sqlite3") as database:
            original = database.execute(
                "SELECT record FROM runtime_profiles WHERE id='python-profile'"
            ).fetchone()[0]
            database.execute(
                "UPDATE runtime_profiles SET record='{' WHERE id='python-profile'"
            )
        before = profile_rows(state)
        try:
            refused = self.backend.request(
                "POST",
                "/api/profiles",
                body={"id": "python-profile", "name": "Replacement", "model": "M"},
                headers=self.headers(),
            )
            self.assertEqual(refused.status, 500, refused.body[:1000])
            self.assertEqual(
                refused.json()["error"], "Invalid stored object profiles/python-profile",
            )
            self.assertEqual(profile_rows(state), before)
        finally:
            with sqlite3.connect(state / "canvas.sqlite3") as database:
                database.execute(
                    "UPDATE runtime_profiles SET record=? WHERE id='python-profile'", (original,),
                )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_missing_profiles_table_reads_empty_and_refuses_writes(self) -> None:
        state = self.backend.state
        schema = profile_table(state)
        self.assertIsNotNone(schema)
        rows = profile_rows(state)
        with sqlite3.connect(state / "canvas.sqlite3") as database:
            database.execute("DROP TABLE runtime_profiles")
        try:
            listed = self.backend.request("GET", "/api/profiles", headers=self.headers())
            self.assertEqual(listed.status, 200, listed.body[:1000])
            self.assertEqual(listed.json(), {"profiles": []})

            refused = self.backend.request(
                "POST",
                "/api/profiles",
                body={
                    "id": "not-written",
                    "name": "Profile",
                    "role": "reviewer",
                    "model": "fixture-model",
                    "effort": None,
                    "instructions": "",
                },
                headers=self.headers(),
            )
            self.assertEqual(refused.status, 501, refused.body[:1000])
            self.assertEqual(
                refused.json()["error"],
                "Not implemented: requires store-bootstrap/workspace-profiles-schema",
            )
            self.assertIsNone(profile_table(state))
        finally:
            with sqlite3.connect(state / "canvas.sqlite3") as database:
                database.execute(schema[0])
                database.executemany(
                    "INSERT INTO runtime_profiles(id, record) VALUES (?, ?)", rows,
                )


if __name__ == "__main__":
    unittest.main()
