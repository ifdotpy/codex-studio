#!/usr/bin/env python3
"""Sync preference merge behavior and declared partial operations."""

from __future__ import annotations

import json
import os
import unittest

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from sync_http_contract_support import prepare_sync_store_for_rust_contract


def sync_state_snapshot(state):
    import sqlite3

    database_path = state / "canvas.sqlite3"
    uri = f"file:{database_path}?mode=ro"
    with sqlite3.connect(uri, uri=True) as database:
        objects = database.execute(
            "SELECT type,name,sql FROM sqlite_master "
            "WHERE name LIKE 'sync_%' OR name IN "
            "('runtime_transcript_revisions','runtime_event_created_id','sqlite_sequence') "
            "ORDER BY type,name"
        ).fetchall()
        tables = [
            name for kind, name, _ in objects
            if kind == "table" and (name.startswith("sync_") or name == "runtime_transcript_revisions")
        ]
        rows = {
            name: sorted(database.execute(f'SELECT * FROM "{name}"').fetchall(), key=repr)
            for name in tables
        }
        if any(name == "sqlite_sequence" for _, name, _ in objects):
            rows["sqlite_sequence"] = sorted(
                database.execute("SELECT name,seq FROM sqlite_sequence").fetchall(), key=repr
            )
        return objects, rows


class SyncDocumentsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        prepare_sync_store_for_rust_contract(cls.backend)
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            prepare_sync_store_for_rust_contract(cls.backend, seed_python_rows=True)

    def _common_headers(self):
        backend = self.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_stored_preferences_reject_extra_envelope_members_without_writes(self) -> None:
        self._assert_invalid_stored_preference(
            '{"collection":"uiPreferences","id":"user",'
            '"value":{"fields":{},"unexpected":true}}'
        )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_stored_preferences_reject_theme_outside_allowed_set_without_writes(self) -> None:
        self._assert_invalid_stored_preference(
            '{"collection":"uiPreferences","id":"user","value":{"fields":'
            '{"[\\"codex-studio-preferences-v1\\",\\"theme\\"]":'
            '{"value":"ultraviolet","timestamp":1,"writer":"device"}}}}'
        )

    def _assert_invalid_stored_preference(self, payload: str) -> None:
        import sqlite3

        backend = self.backend
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            original = database.execute(
                "SELECT payload FROM sync_entities "
                "WHERE collection='uiPreferences' AND id='user'"
            ).fetchone()[0]
            database.execute(
                "UPDATE sync_entities SET payload=? "
                "WHERE collection='uiPreferences' AND id='user'",
                (payload,),
            )
        before = sync_state_snapshot(backend.state)
        response = backend.request(
            "POST", "/api/sync/preferences",
            body={"scope": "user", "fields": {}},
            headers=self._common_headers(),
        )
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertEqual(
            response.json()["error"],
            "invalid stored sync object uiPreferences/user",
        )
        self.assertEqual(sync_state_snapshot(backend.state), before)
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            database.execute(
                "UPDATE sync_entities SET payload=? "
                "WHERE collection='uiPreferences' AND id='user'",
                (original,),
            )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_stored_draft_pull_rejects_invalid_identity_without_writes(self) -> None:
        import sqlite3

        backend = self.backend
        database_path = backend.state / "canvas.sqlite3"
        headers = self._common_headers()
        with sqlite3.connect(database_path) as database:
            original = database.execute(
                "SELECT seq,id,payload,deleted FROM sync_documents WHERE scope='drafts'"
            ).fetchall()
        invalid = [
            ("stored-empty", "{}"),
            ("stored-null", "null"),
            ("stored-conflict", '{"device":"other","session":"chat"}'),
        ]
        try:
            for index, (draft_id, payload) in enumerate(invalid, start=10_000):
                with sqlite3.connect(database_path) as database:
                    database.execute("DELETE FROM sync_documents WHERE scope='drafts'")
                    database.execute(
                        "INSERT INTO sync_documents(seq,scope,id,payload,deleted) "
                        "VALUES (?, 'drafts', ?, ?, 0)",
                        (index, draft_id, payload),
                    )
                before = sync_state_snapshot(backend.state)
                response = backend.request(
                    "GET", "/api/sync/pull?scope=drafts&after=0", headers=headers,
                )
                self.assertEqual(response.status, 500, response.body[:1000])
                self.assertEqual(
                    response.json()["error"],
                    f"Invalid stored sync object drafts/{draft_id}",
                )
                self.assertEqual(sync_state_snapshot(backend.state), before)
            with sqlite3.connect(database_path) as database:
                database.execute("DELETE FROM sync_documents WHERE scope='drafts'")
                database.executemany(
                    "INSERT INTO sync_documents(seq,scope,id,payload,deleted) "
                    "VALUES (?, 'drafts', ?, ?, ?)",
                    original,
                )
            with sqlite3.connect(database_path) as database:
                database.execute(
                    "INSERT INTO sync_documents(seq,scope,id,payload,deleted) "
                    "VALUES (20000,'drafts','phone:chat',?,0)",
                    ('{"device":"phone","session":"chat","extra":{"pass":true}}',),
                )
            response = backend.request(
                "GET", "/api/sync/pull?scope=drafts&after=0", headers=headers,
            )
            self.assertEqual(response.status, 200, response.body[:1000])
            document = next(item for item in response.json()["documents"] if item["id"] == "phone:chat")
            self.assertEqual(json.loads(document["payload"])["extra"], {"pass": True})
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute("DELETE FROM sync_documents WHERE scope='drafts'")
                database.executemany(
                    "INSERT INTO sync_documents(seq,scope,id,payload,deleted) "
                    "VALUES (?, 'drafts', ?, ?, ?)",
                    original,
                )

    def test_preferences_validate_merge_and_partial_dependencies(self) -> None:
        backend = self.backend
        rust_backend_configured = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        common = {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }
        theme_key = '["codex-studio-preferences-v1","theme"]'
        font_key = '["codex-studio-preferences-v1","mainFontSize"]'

        if rust_backend_configured:
            import sqlite3

            before_overflowing_cursor = sync_state_snapshot(backend.state)
            overflowing_cursor = backend.request(
                "GET", "/api/sync/pull?scope=drafts&after=9223372036854775808",
                headers=common,
            )
            self.assertEqual(overflowing_cursor.status, 400, overflowing_cursor.body[:1000])
            self.assertEqual(overflowing_cursor.json()["error"], "Invalid request")
            self.assertEqual(sync_state_snapshot(backend.state), before_overflowing_cursor)

            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute("UPDATE sync_generation SET value=5 WHERE id=1")
            before_protocol = sync_state_snapshot(backend.state)
            protocol = backend.request("GET", "/api/sync/protocol", headers=common)
            self.assertEqual(protocol.status, 501, protocol.body[:1000])
            self.assertEqual(
                protocol.json()["error"],
                "Not implemented: requires sync-policy/runtime-capabilities",
            )
            self.assertEqual(sync_state_snapshot(backend.state), before_protocol)

        def entity_preferences_document():
            before = sync_state_snapshot(backend.state) if rust_backend_configured else None
            response = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1"
            )
            if rust_backend_configured:
                self.assertEqual(response.status, 501, response.body[:1000])
                self.assertIn(
                    response.json()["error"],
                    {
                        "Not implemented: requires sync-policy/entity-seeding, "
                        "sync-policy/entity-window-maintenance, transcript-service/pull",
                    },
                )
                self.assertEqual(sync_state_snapshot(backend.state), before)
                return None
            self.assertEqual(response.status, 200, response.body[:1000])
            return next(
                document for document in response.json()["documents"]
                if document["id"] == "entity:uiPreferences:user"
            )

        first = backend.request(
            "POST",
            "/api/sync/preferences",
            body={"scope": "user", "fields": {
                theme_key: {"value": "dark", "timestamp": 10, "writer": "phone-a"},
            }},
            headers=common,
        )
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(first.json()["fields"][theme_key]["value"], "dark")
        if rust_backend_configured:
            self.assertEqual(
                first.json()["fields"]['["codex-studio-preferences-v1","typography"]']["value"],
                "custom",
            )
            python_drafts = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
            self.assertEqual(python_drafts.status, 200, python_drafts.body[:1000])
            self.assertEqual(
                python_drafts.json()["workspaceId"],
                backend.identity["workspaceId"],
            )
            self.assertIn(
                "python-device:python-session",
                [document["id"] for document in python_drafts.json()["documents"]],
            )

        entity_preferences_document()

        replay = backend.request(
            "POST",
            "/api/sync/preferences",
            body={"scope": "user", "fields": {
                theme_key: {"value": "dark", "timestamp": 10, "writer": "phone-a"},
            }},
            headers=common,
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["fields"], first.json()["fields"])

        merged = backend.request(
            "POST",
            "/api/sync/preferences",
            body={"scope": "user", "fields": {
                font_key: {"value": 18, "timestamp": 20, "writer": "phone-a"},
                theme_key: {"value": "light", "timestamp": 9, "writer": "phone-z"},
            }},
            headers=common,
        )
        self.assertEqual(merged.status, 200, merged.body[:1000])
        self.assertEqual(merged.json()["fields"][theme_key]["value"], "dark")
        self.assertEqual(merged.json()["fields"][font_key]["value"], 18)

        current_doc = entity_preferences_document()
        before_stable_replay = (
            sync_state_snapshot(backend.state) if rust_backend_configured else None
        )
        stable_replay = backend.request(
            "POST",
            "/api/sync/preferences",
            body={"scope": "user", "fields": {
                font_key: {"value": 18, "timestamp": 20, "writer": "phone-a"},
            }},
            headers=common,
        )
        self.assertEqual(stable_replay.status, 200, stable_replay.body[:1000])
        if rust_backend_configured:
            self.assertEqual(sync_state_snapshot(backend.state), before_stable_replay)
        stable_doc = entity_preferences_document()
        if not rust_backend_configured:
            self.assertEqual(stable_doc["seq"], current_doc["seq"])
        else:
            import sqlite3

            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                original_preference = database.execute(
                    "SELECT payload FROM sync_entities "
                    "WHERE collection='uiPreferences' AND id='user'"
                ).fetchone()[0]
                database.execute(
                    "UPDATE sync_entities SET payload=? "
                    "WHERE collection='uiPreferences' AND id='user'",
                    ('{"collection":"agent","id":"wrong","value":{"fields":{}}}',),
                )
            before_invalid_preference = sync_state_snapshot(backend.state)
            invalid_stored_preference = backend.request(
                "POST", "/api/sync/preferences", body={"scope": "user", "fields": {}},
                headers=common,
            )
            self.assertEqual(invalid_stored_preference.status, 500)
            self.assertIn("uiPreferences/user", invalid_stored_preference.json()["error"])
            self.assertEqual(sync_state_snapshot(backend.state), before_invalid_preference)
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute(
                    "UPDATE sync_entities SET payload=? "
                    "WHERE collection='uiPreferences' AND id='user'",
                    (original_preference,),
                )

        invalid_value = backend.request(
            "POST", "/api/sync/preferences",
            body={"scope": "user", "fields": {
                theme_key: {"value": "ultraviolet", "timestamp": 30, "writer": "phone-a"},
            }},
            headers=common,
        )
        self.assertEqual(invalid_value.status, 400)
        wrong_scope = backend.request(
            "POST", "/api/sync/preferences",
            body={"scope": "server", "fields": {
                theme_key: {"value": "dark", "timestamp": 30, "writer": "phone-a"},
            }},
            headers=common,
        )
        self.assertEqual(wrong_scope.status, 400)
        malformed_key = backend.request(
            "POST", "/api/sync/preferences",
            body={"scope": "user", "fields": {
                ' ["codex-studio-preferences-v1","theme"]': {
                    "value": "dark", "timestamp": 30, "writer": "phone-a",
                },
            }},
            headers=common,
        )
        self.assertEqual(malformed_key.status, 400)

        before_transcript = (
            sync_state_snapshot(backend.state) if rust_backend_configured else None
        )
        transcript = backend.request("GET", "/api/sync/pull?scope=transcript:agent-a")
        if not rust_backend_configured:
            self.assertEqual(transcript.status, 200, transcript.body[:1000])
        else:
            self.assertEqual(transcript.status, 501)
            self.assertEqual(
                transcript.json()["error"],
                "Not implemented: requires sync-policy/entity-seeding, "
                "sync-policy/entity-window-maintenance, transcript-service/pull",
            )
            self.assertEqual(sync_state_snapshot(backend.state), before_transcript)

        draft = {
            "rows": [{"newDocumentState": {
                "id": "rollback-device:rollback-session",
                "payload": json.dumps({
                    "device": "rollback-device", "session": "rollback-session",
                    "text": "survives Rust to Python restart",
                }),
            }}],
        }
        before_draft_write = (
            sync_state_snapshot(backend.state) if rust_backend_configured else None
        )
        pushed = backend.request(
            "POST", "/api/sync/drafts", body=draft,
            headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
        )
        if rust_backend_configured:
            self.assertEqual(pushed.status, 501, pushed.body[:1000])
            self.assertEqual(
                pushed.json()["error"],
                "Not implemented: requires resource-policy/publish-drafts",
            )
            self.assertEqual(sync_state_snapshot(backend.state), before_draft_write)
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute(
                    "INSERT OR REPLACE INTO sync_documents(scope,id,payload,deleted) "
                    "VALUES ('drafts','bad-json','not-json',0)"
                )
            before_invalid_draft = sync_state_snapshot(backend.state)
            invalid_stored_draft = backend.request(
                "GET", "/api/sync/pull?scope=drafts&after=0", headers=common,
            )
            self.assertEqual(invalid_stored_draft.status, 500)
            self.assertIn("drafts/bad-json", invalid_stored_draft.json()["error"])
            self.assertNotIn("not-json", invalid_stored_draft.json()["error"])
            self.assertEqual(sync_state_snapshot(backend.state), before_invalid_draft)
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute(
                    "DELETE FROM sync_documents WHERE scope='drafts' AND id='bad-json'"
                )
        else:
            self.assertEqual(pushed.status, 200, pushed.body[:1000])
            replay = backend.request(
                "POST", "/api/sync/drafts", body=draft,
                headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
            )
            self.assertEqual(replay.status, 200, replay.body[:1000])
            self.assertEqual(replay.json(), [])
            draft_snapshot = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
            self.assertEqual(draft_snapshot.status, 200, draft_snapshot.body[:1000])
            self.assertIn(
                "rollback-device:rollback-session",
                [document["id"] for document in draft_snapshot.json()["documents"]],
            )

        if not rust_backend_configured:
            conflicting = {
                "rows": [{"newDocumentState": {
                    "id": "rollback-device:rollback-session",
                    "payload": json.dumps({
                        "device": "rollback-device", "session": "rollback-session",
                        "text": "conflicting replay",
                    }),
                }}],
            }
            conflict = backend.request(
                "POST", "/api/sync/drafts", body=conflicting,
                headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
            )
            self.assertEqual(conflict.status, 200, conflict.body[:1000])
            self.assertEqual(conflict.json()[0]["id"], "rollback-device:rollback-session")
            self.assertEqual(
                json.loads(conflict.json()[0]["payload"])["text"],
                "survives Rust to Python restart",
            )
        invalid_identity = {
            "rows": [{"newDocumentState": {
                "id": "wrong-device:rollback-session",
                "payload": json.dumps({
                    "device": "rollback-device", "session": "rollback-session",
                    "text": "invalid identity",
                }),
            }}],
        }
        invalid_draft = backend.request(
            "POST", "/api/sync/drafts", body=invalid_identity,
            headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
        )
        self.assertEqual(invalid_draft.status, 400)

        configured = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
        if configured is None:
            backend.stop()
            backend.start()
            python_drafts = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
            self.assertEqual(python_drafts.status, 200, python_drafts.body[:1000])
            python_draft_ids = [document["id"] for document in python_drafts.json()["documents"]]
            self.assertIn("rollback-device:rollback-session", python_draft_ids)
            python_entities = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1"
            )
            self.assertEqual(python_entities.status, 200, python_entities.body[:1000])
            python_preferences = next(
                document for document in python_entities.json()["documents"]
                if document["id"] == "entity:uiPreferences:user"
            )
            stored_fields = json.loads(python_preferences["payload"])["value"]["fields"]
            self.assertEqual(stored_fields[theme_key]["value"], "dark")
            self.assertEqual(stored_fields[font_key]["value"], 18)

        if configured is None:
            return
        import sqlite3
        backend.stop()
        workspace_id = backend.identity["workspaceId"]
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            database.executescript("""
                CREATE TRIGGER sync_watch_contract BEFORE INSERT ON runtime_events BEGIN
                    UPDATE sync_generation SET value=value+1 WHERE id=1;
                END;
            """)
            trigger_names = database.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger' "
                "AND name LIKE 'sync_%' AND name!='sync_watch_contract'"
            ).fetchall()
            for (name,) in trigger_names:
                database.execute(f'DROP TRIGGER "{name}"')
            for table in (
                "sync_documents",
                "sync_entities",
                "sync_entity_meta",
                "sync_versions",
                "sync_generation",
                "runtime_transcript_revisions",
            ):
                database.execute(f'DROP TABLE IF EXISTS "{table}"')
        backend.start()
        self.assertEqual(backend.identity["workspaceId"], workspace_id)
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            self.assertTrue(database.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_documents'"
            ).fetchone())
            self.assertIsNone(database.execute(
                "SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='sync_watch_contract'"
            ).fetchone())
        bootstrap_headers = {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
            "X-Canvas-Workspace": workspace_id,
        }
        protocol_after_bootstrap = backend.request(
            "GET", "/api/sync/protocol", headers=bootstrap_headers,
        )
        self.assertEqual(protocol_after_bootstrap.status, 501)
        self.assertEqual(
            protocol_after_bootstrap.json()["error"],
            "Not implemented: requires sync-policy/runtime-capabilities",
        )
        pull_after_bootstrap = backend.request(
            "GET", "/api/sync/pull?scope=drafts&after=0", headers=bootstrap_headers,
        )
        self.assertEqual(pull_after_bootstrap.status, 200, pull_after_bootstrap.body[:1000])
        self.assertEqual(pull_after_bootstrap.json()["workspaceId"], workspace_id)
        preference_after_bootstrap = backend.request(
            "POST", "/api/sync/preferences",
            body={"scope": "user", "fields": {
                theme_key: {"value": "light", "timestamp": 31, "writer": "bootstrap"},
            }},
            headers=bootstrap_headers,
        )
        self.assertEqual(preference_after_bootstrap.status, 200)
        draft_after_bootstrap = backend.request(
            "POST", "/api/sync/drafts",
            body={"rows": [{"newDocumentState": {
                "id": "bootstrap-device:bootstrap-session",
                "payload": json.dumps({
                    "device": "bootstrap-device", "session": "bootstrap-session", "text": "x",
                }),
            }}]},
            headers=bootstrap_headers,
        )
        self.assertEqual(draft_after_bootstrap.status, 501)
        self.assertEqual(
            draft_after_bootstrap.json()["error"],
            "Not implemented: requires resource-policy/publish-drafts",
        )

if __name__ == "__main__":
    unittest.main(verbosity=2)
