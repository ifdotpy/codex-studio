#!/usr/bin/env python3
"""HTTP contracts for profiles and progress panel operations."""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))

from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from codex_progress import provision_progress


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
                lead = cls.backend.request(
                    "POST", "/api/leads",
                    body={"cwd": str(cls.backend.cwd), "workspaceMode": "worktree"},
                    headers=cls.write_headers(),
                )
                if lead.status != 200:
                    raise AssertionError(f"Python agent seed failed: HTTP {lead.status}")
                cls.lead_id = lead.json()["id"]
                layout_agent = cls.backend.request(
                    "POST", "/api/leads",
                    body={"cwd": str(cls.backend.cwd), "workspaceMode": "worktree"},
                    headers=cls.write_headers(),
                )
                if layout_agent.status != 200:
                    raise AssertionError(f"Python layout agent seed failed: HTTP {layout_agent.status}")
                cls.layout_agent = layout_agent.json()["id"]
                cls._seed_python_layout()
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
            lead = cls.backend.request(
                "POST", "/api/leads",
                body={"cwd": str(cls.backend.cwd), "workspaceMode": "worktree"},
                headers=cls.write_headers(),
            )
            if lead.status != 200:
                raise AssertionError(f"Python agent seed failed: HTTP {lead.status}")
            cls.lead_id = lead.json()["id"]
            layout_agent = cls.backend.request(
                "POST", "/api/leads",
                body={"cwd": str(cls.backend.cwd), "workspaceMode": "worktree"},
                headers=cls.write_headers(),
            )
            if layout_agent.status != 200:
                raise AssertionError(f"Python layout agent seed failed: HTTP {layout_agent.status}")
            cls.layout_agent = layout_agent.json()["id"]
            cls._seed_python_layout()

    @classmethod
    def headers(cls):
        backend = cls.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    @classmethod
    def write_headers(cls):
        return {
            **cls.headers(),
            "X-Canvas-Workspace": cls.backend.identity["workspaceId"],
        }

    @classmethod
    def _seed_python_layout(cls):
        path = provision_progress(cls.backend.state, cls.layout_agent)
        path.write_text("# Python layout seed\n", encoding="utf-8")
        panel = cls.backend.request(
            "GET", f"/api/panel?agent={cls.layout_agent}", headers=cls.headers(),
        )
        if panel.status != 200:
            raise AssertionError(f"Python panel seed failed: HTTP {panel.status}")
        measurement = {
            "agent": cls.layout_agent,
            "revision": panel.json()["revision"],
            "client": "phone",
            "sequence": 1,
            "renderer": "progress-markdown-v2",
            "width": 320,
            "height": 150,
            "contentWidth": 100,
            "contentHeight": 40,
            "overflowX": 0,
            "overflowY": 0,
            "totalLines": 1,
            "visibleLines": 1,
            "lastVisibleLine": "# Python layout seed",
            "lastVisibleHeading": "# Python layout seed",
            "fits": True,
            "reason": None,
        }
        saved = cls.backend.request(
            "POST", "/api/panel/layout", body=measurement, headers=cls.headers(),
        )
        if saved.status != 200:
            raise AssertionError(f"Python layout seed failed: HTTP {saved.status}")

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

    def test_progress_panel_reads_python_written_file_and_missing_read_is_read_only(self) -> None:
        backend = self.backend
        path = backend.state / "progress" / self.lead_id / "PROGRESS.md"
        missing = backend.request(
            "GET", f"/api/panel?agent={self.lead_id}", headers=self.headers(),
        )
        self.assertEqual(missing.status, 200, missing.body[:1000])
        self.assertFalse(missing.json()["exists"])
        self.assertFalse(path.exists(), "Reading a missing panel must not provision it")

        provision_progress(backend.state, self.lead_id)
        path.write_text("# Python written\n\n- Preserve this content\n", encoding="utf-8")
        response = backend.request(
            "GET", f"/api/panel?agent={self.lead_id}", headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        panel = response.json()
        self.assertEqual(panel["markdown"], "# Python written\n\n- Preserve this content\n")
        self.assertEqual(panel["path"], str(path.absolute()))
        self.assertTrue(panel["exists"])
        self.assertIsNotNone(panel["revision"])

    def test_layout_write_replays_and_stale_revision_refuses_without_write(self) -> None:
        backend = self.backend
        path = backend.state / "progress" / self.layout_agent / "PROGRESS.md"
        panel = backend.request(
            "GET", f"/api/panel?agent={self.layout_agent}", headers=self.headers(),
        )
        self.assertEqual(panel.status, 200, panel.body[:1000])
        body = {
            "agent": self.layout_agent,
            "revision": panel.json()["revision"],
            "client": "desktop",
            "sequence": 1,
            "renderer": "progress-markdown-v2",
            "width": 320,
            "height": 150,
            "contentWidth": 100,
            "contentHeight": 40,
            "overflowX": 0,
            "overflowY": 0,
            "totalLines": 1,
            "visibleLines": 1,
            "lastVisibleLine": "# Python layout seed",
            "lastVisibleHeading": "# Python layout seed",
            "fits": True,
            "reason": None,
        }
        saved = backend.request("POST", "/api/panel/layout", body=body, headers=self.headers())
        self.assertEqual(saved.status, 200, saved.body[:1000])
        self.assertEqual(
            [(row["client"], row["sequence"]) for row in saved.json()["reports"]],
            [("phone", 1), ("desktop", 1)],
        )
        feedback_path = path.with_name("PROGRESS.layout.json")
        python_written = json.loads(feedback_path.read_text(encoding="utf-8"))
        python_written["reports"][1]["width"] = 320.0
        feedback_path.chmod(0o600)
        feedback_path.write_text(json.dumps(python_written, indent=2) + "\n", encoding="utf-8")
        replay = backend.request("POST", "/api/panel/layout", body=body, headers=self.headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["reports"][1]["width"], 320)
        before = feedback_path.read_bytes()
        path.write_text("# New revision\n", encoding="utf-8")
        stale = backend.request("POST", "/api/panel/layout", body=body, headers=self.headers())
        self.assertEqual(stale.status, 409, stale.body[:1000])
        self.assertEqual(feedback_path.read_bytes(), before)

        invalid = backend.request(
            "POST", "/api/panel/layout",
            body={key: value for key, value in body.items() if key != "revision"},
            headers=self.headers(),
        )
        self.assertEqual(invalid.status, 400, invalid.body[:1000])
        self.assertEqual(invalid.json()["location"], ["body", "revision"])
        self.assertEqual(feedback_path.read_bytes(), before)

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
