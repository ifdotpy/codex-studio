#!/usr/bin/env python3
"""Sync preference merge behavior and declared partial operations."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
import uuid

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
        cls._prepare_python_entity_tombstone()

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_entity_seeding_matches_python_for_persisted_state_and_is_idempotent(self) -> None:
        import sqlite3

        backend = self.backend
        backend.stop()
        database_path = backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            previous_entities = database.execute("SELECT * FROM sync_entities").fetchall()
            previous_meta = database.execute("SELECT * FROM sync_entity_meta").fetchall()

        source = Path(__file__).resolve().parents[1] / "src"
        python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
        suffix = uuid.uuid4().hex
        agent_a, agent_b = f"sync-diff-{suffix}-a", f"sync-diff-{suffix}-b"
        room_id = f"sync-diff-{suffix}-room"
        complaint_id = f"sync-diff-{suffix}-complaint"
        project_id = f"sync-diff-{suffix}-project"
        project_path = str((backend.cwd / project_id).resolve())
        request_id = f"sync-diff-{suffix}-request"
        rule_id = f"sync-diff-{suffix}-rule"
        work_id = f"sync-diff-{suffix}-work"
        task_id = f"sync-diff-{suffix}-task"
        monitor_id = f"sync-diff-{suffix}-monitor"
        event_id = f"sync-diff-{suffix}-event"
        chat_id = f"sync-diff-{suffix}-chat"
        edge_id = f"sync-diff-{suffix}-edge"
        message_id = f"sync-diff-{suffix}-message"
        fixtures = {
            "records": {
                "runtime_agents": [
                    [agent_a, {"id": agent_a, "kind": "agent", "isLead": True, "parentId": None,
                     "rootId": agent_a, "cwd": project_path, "status": "idle"}],
                    [agent_b, {"id": agent_b, "kind": "agent", "isLead": True, "parentId": None,
                     "rootId": agent_b, "cwd": project_path, "status": "idle"}],
                ],
                "runtime_rooms": [[room_id, {"id": room_id, "kind": "private", "members": []}]],
                "runtime_complaints": [[complaint_id, {"id": complaint_id}]],
                "runtime_projects": [[project_id, {
                    "id": project_id,
                    "path": project_path,
                    "peerTeams": [{"id": f"sync-diff-{suffix}-team", "name": "Sync test",
                                   "members": [agent_a, agent_b]}],
                    "peerTeamsRevision": 7,
                }]],
                "runtime_requests": [[request_id, {"id": request_id, "status": "pending"}]],
                "runtime_rules": [[rule_id, {"id": rule_id}]],
                "runtime_work": [[work_id, {"id": work_id}]],
                "runtime_tasks": [[task_id, {
                    "id": task_id, "agent": agent_a, "status": "running", "created": 10,
                }]],
                "runtime_monitors": [[monitor_id, {
                    "id": monitor_id, "agent": agent_a, "status": "running", "created": 10,
                }]],
            },
            "events": [[event_id, agent_a, "sync-test", "delivered", 10,
                        None, "", 0, None]],
            "groups": [[chat_id, "Sync test chat", "[]"]],
            "edges": [[edge_id, agent_a, chat_id, "chat"]],
            "messages": [[message_id, chat_id, agent_a, "chat tail", 10, "{}"]],
        }
        fixture_keys = {
            **{table: [row[0] for row in rows] for table, rows in fixtures["records"].items()},
            "runtime_events": [event_id],
            "groups": [chat_id],
            "graph_edges": [edge_id],
            "messages": [message_id],
        }
        fixture_json = json.dumps(fixtures, separators=(",", ":"))
        fixture_keys_json = json.dumps(fixture_keys, separators=(",", ":"))
        seed_python = f"""
import json
import os
import sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_canvas import Canvas
from codex_sync_entities import seed
from codex_peer_teams import sync_entities
canvas = Canvas(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
fixtures = json.loads({fixture_json!r})
def table_exists(db, name):
    return db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)).fetchone() is not None
def insert_records(db, table, rows):
    if table_exists(db, table):
        db.executemany(f"INSERT OR REPLACE INTO {{table}}(id,record) VALUES (?,?)",
                       [(key, json.dumps(record, sort_keys=True, separators=(",", ":")))
                        for key, record in rows])
def rows(db, sequences):
    persisted_agents = {{row[0] for row in db.execute("SELECT id FROM runtime_agents")}} if table_exists(db, "runtime_agents") else set()
    persisted_edges = {{row[0] for row in db.execute("SELECT id FROM graph_edges")}} if table_exists(db, "graph_edges") else set()
    return [{{"collection": collection, "id": key,
             "payload": json.loads(payload) if payload is not None else None,
             "deleted": bool(deleted), **({{"seq": seq}} if sequences else {{}})}}
            for collection,key,seq,payload,deleted in db.execute(
                "SELECT collection,id,seq,payload,deleted FROM sync_entities ORDER BY collection,id")
            if (collection != "agent" or key in persisted_agents)
            and (collection != "edge" or key in persisted_edges)]
with canvas.connect() as db:
    db.execute("DELETE FROM sync_entities")
    db.execute("DELETE FROM sync_entity_meta WHERE key IN ('seeded','agent_organization_fields', 'task_window_migrated', 'event_window_seq')")
    for table, records in fixtures["records"].items():
        insert_records(db, table, records)
    if table_exists(db, "runtime_events"):
        db.executemany("INSERT OR REPLACE INTO runtime_events(id,agent,kind,status,created,error,text,epoch,turn_id) VALUES (?,?,?,?,?,?,?,?,?)",
                       fixtures["events"])
    if table_exists(db, "groups"):
        db.executemany("INSERT OR REPLACE INTO groups(id,name,members) VALUES (?,?,?)", fixtures["groups"])
    if table_exists(db, "graph_edges"):
        db.executemany("INSERT OR REPLACE INTO graph_edges(id,source,target,kind) VALUES (?,?,?,?)", fixtures["edges"])
    if table_exists(db, "messages"):
        db.executemany("INSERT OR REPLACE INTO messages(id,room,author,text,at,deliveries) VALUES (?,?,?,?,?,?)", fixtures["messages"])
    seed(db, runtime_owner=None, canvas_owner=canvas)
    sync_entities(None, db)
    first_with_sequences = rows(db, True)
    first = rows(db, False)
    seed(db, runtime_owner=None, canvas_owner=canvas)
    sync_entities(None, db)
    second_with_sequences = rows(db, True)
    second = rows(db, False)
    if first_with_sequences != second_with_sequences:
        raise AssertionError("Python second seed changed entity rows or sequences")
    if first != second:
        raise AssertionError("Python entity seeding was not idempotent")
    print(json.dumps(first, sort_keys=True, separators=(",", ":")))
    db.execute("DELETE FROM sync_entities")
    db.execute("DELETE FROM sync_entity_meta WHERE key IN ('seeded','agent_organization_fields', 'task_window_migrated', 'event_window_seq')")
"""
        try:
            try:
                seed_result = subprocess.run(
                    [python, "-B", "-c", seed_python],
                    cwd=backend.cwd,
                    env=backend._environment(),
                    check=True,
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
            except subprocess.CalledProcessError as error:
                self.fail(f"Python persisted-state seed failed: {error.stderr[-2000:]}")
            expected = json.loads(seed_result.stdout)
            backend.start()
            headers = self._common_headers()
            try:
                pulled = backend.request(
                    "GET", "/api/sync/pull?scope=state:entities:v1&after=0&limit=500&fresh=1&reset=1",
                    headers=headers,
                )
            except Exception as error:
                self.fail(f"Rust entity pull transport failed: {error}; backend stderr: {backend.stderr}")
            self.assertEqual(pulled.status, 200, pulled.body[:1000])
            with sqlite3.connect(database_path) as database:
                table_exists = lambda name: database.execute(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,),
                ).fetchone() is not None
                persisted_agents = (
                    {row[0] for row in database.execute("SELECT id FROM runtime_agents")}
                    if table_exists("runtime_agents") else set()
                )
                persisted_edges = (
                    {row[0] for row in database.execute("SELECT id FROM graph_edges")}
                    if table_exists("graph_edges") else set()
                )
                actual = [
                    {"collection": collection, "id": key,
                     "payload": json.loads(payload) if payload is not None else None,
                     "deleted": bool(deleted)}
                    for collection,key,payload,deleted in database.execute(
                        "SELECT collection,id,payload,deleted FROM sync_entities ORDER BY collection,id")
                    if (collection != "agent" or key in persisted_agents)
                    and (collection != "edge" or key in persisted_edges)
                ]
            if actual != expected:
                actual_by_key = {(row["collection"], row["id"]): row for row in actual}
                expected_by_key = {(row["collection"], row["id"]): row for row in expected}
                keys = actual_by_key.keys() | expected_by_key.keys()
                differences = {
                    f"{collection}:{key}": {
                        "expected": expected_by_key.get((collection, key)),
                        "actual": actual_by_key.get((collection, key)),
                    }
                    for collection, key in sorted(keys)
                    if actual_by_key.get((collection, key)) != expected_by_key.get((collection, key))
                }
                self.fail("Python/Rust persisted-state seed mismatch: " + json.dumps(differences, sort_keys=True))
            before_repeat = sync_state_snapshot(backend.state)
            repeated = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&limit=500&fresh=1&reset=1",
                headers=headers,
            )
            self.assertEqual(repeated.status, 200, repeated.body[:1000])
            self.assertEqual(sync_state_snapshot(backend.state), before_repeat)
        finally:
            backend.stop()
            cleanup_fixtures = f"""
import json, os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_canvas import Canvas
keys = json.loads({fixture_keys_json!r})
canvas = Canvas(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
with canvas.connect() as db:
    for table, ids in keys.items():
        if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)).fetchone():
            db.executemany(f"DELETE FROM {{table}} WHERE id=?", [(key,) for key in ids])
"""
            subprocess.run(
                [python, "-B", "-c", cleanup_fixtures],
                cwd=backend.cwd,
                env=backend._environment(),
                check=True,
                timeout=30,
            )
            with sqlite3.connect(database_path) as database:
                database.execute("DELETE FROM sync_entities")
                database.executemany(
                    "INSERT INTO sync_entities VALUES(?,?,?,?,?,?)", previous_entities,
                )
                database.execute("DELETE FROM sync_entity_meta")
                database.executemany(
                    "INSERT INTO sync_entity_meta VALUES(?,?)", previous_meta,
                )
            backend.start()

    @classmethod
    def _prepare_python_entity_tombstone(cls) -> None:
        backend = cls.backend
        backend.stop()
        source = Path(__file__).resolve().parents[1] / "src"
        python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
        prepare = f"""
import os
import sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_canvas import Canvas
from codex_sync import SyncStore
from codex_sync_entities import put
canvas = Canvas(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
store = SyncStore(canvas.connect, canvas.transcript, canvas=canvas)
store.entity_pull(0, 100, True, 0, True, None)
with canvas.connect() as db:
    db.execute("BEGIN IMMEDIATE")
    put(db, "agent", "python-priority-agent", {{
        "id": "python-priority-agent",
        "kind": "agent",
        "name": "Python priority",
        "source": "managed",
        "status": "idle",
    }})
    put(db, "agent", "python-deleted-agent", {{}}, deleted=True)
"""
        subprocess.run(
            [python, "-B", "-c", prepare],
            cwd=backend.cwd,
            env=backend._environment(),
            check=True,
            timeout=30,
        )
        backend.start()

    def test_draft_payload_schema_matches_python_model(self) -> None:
        from studio_api.schema import bound_integer_schemas
        from studio_api.sync.models import DraftPayload

        schema_path = (
            Path(__file__).resolve().parents[3]
            / "packages/runtime-http/draft-payload.schema.json"
        )
        expected = DraftPayload.model_json_schema(mode="validation")
        bound_integer_schemas(expected)
        self.assertEqual(json.loads(schema_path.read_text(encoding="utf-8")), expected)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_nested_draft_payload_rejects_entire_batch_without_writes(self) -> None:
        backend = self.backend
        before = sync_state_snapshot(backend.state)
        response = backend.request(
            "POST",
            "/api/sync/drafts",
            body={"rows": [
                {"newDocumentState": {
                    "id": "batch-device:valid-session",
                    "payload": json.dumps({
                        "device": "batch-device",
                        "session": "valid-session",
                        "text": "valid first row",
                    }),
                }},
                {"newDocumentState": {
                    "id": "batch-device:invalid-session",
                    "payload": json.dumps({
                        "device": "batch-device",
                        "session": "invalid-session",
                        "text": 7,
                    }),
                }},
            ]},
            headers={
                **self._common_headers(),
                "X-Canvas-Workspace": backend.identity["workspaceId"],
            },
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json()["error"], "Invalid request")
        self.assertEqual(sync_state_snapshot(backend.state), before)

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
                    ('{"device":"phone","session":"chat","text":"stored","extra":{"pass":true}}',),
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

    def test_preferences_validate_merge_and_sync_protocol_pull_and_drafts(self) -> None:
        backend = self.backend
        rust_backend_configured = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        common = {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }
        theme_key = '["codex-studio-preferences-v1","theme"]'
        font_key = '["codex-studio-preferences-v1","mainFontSize"]'

        protocol = backend.request("GET", "/api/sync/protocol", headers=common)
        self.assertEqual(protocol.status, 200, protocol.body[:1000])
        protocol_body = protocol.json()
        self.assertEqual(protocol_body["protocolVersion"], 3)
        self.assertEqual(protocol_body["supportedVersions"], [3])
        self.assertEqual(protocol_body["scopes"], [
            "state:entities:v1", "transcript:<agent-id>", "drafts",
        ])
        self.assertEqual(protocol_body["maxEntityPage"], 500)
        self.assertEqual(protocol_body["maxOtherPage"], 100)
        self.assertEqual(protocol_body["maxStreamDocuments"], 100)
        self.assertEqual(protocol_body["maxStreamBytes"], 1_048_576)
        if rust_backend_configured:
            self.assertIn("entitySeedExcluded:canvasAgentThreads", protocol_body["capabilities"])
            self.assertIn("entitySeedExcluded:canvasSpawnEdges", protocol_body["capabilities"])
        else:
            seeded_preference = backend.request(
                "POST", "/api/sync/preferences",
                body={"scope": "user", "fields": {
                    theme_key: {"value": "light", "timestamp": 1, "writer": "python-seed"},
                }},
                headers=common,
            )
            self.assertEqual(seeded_preference.status, 200, seeded_preference.body[:1000])

        first_entity_page = backend.request(
            "GET",
            "/api/sync/pull?scope=state:entities:v1&after=0&limit=1&fresh=1&reset=1",
            headers=common,
        )
        self.assertEqual(first_entity_page.status, 200, first_entity_page.body[:1000])
        first_entity_body = first_entity_page.json()
        initial_high = first_entity_body["initialHigh"]
        max_sequence = first_entity_body["maxSeq"]
        entity_documents = list(first_entity_body["documents"])
        cursor = first_entity_body["checkpoint"]["seq"]
        while cursor < max_sequence:
            page = backend.request(
                "GET",
                "/api/sync/pull?scope=state:entities:v1"
                f"&after={cursor}&limit=1&fresh=1&initialHigh={initial_high}",
                headers=common,
            )
            self.assertEqual(page.status, 200, page.body[:1000])
            body = page.json()
            entity_documents.extend(body["documents"])
            next_cursor = body["checkpoint"]["seq"]
            self.assertGreaterEqual(next_cursor, cursor)
            self.assertEqual(body["maxSeq"], max_sequence)
            cursor = next_cursor
        self.assertEqual(cursor, max_sequence)
        entity_sequences = [document["seq"] for document in entity_documents]
        self.assertEqual(entity_sequences, sorted(set(entity_sequences)))
        self.assertTrue(any(
            document["id"] == "entity:uiPreferences:user"
            for document in entity_documents
        ))
        priority_page = backend.request(
            "GET",
            "/api/sync/pull?scope=state:entities:v1"
            "&after=0&limit=1&fresh=1&reset=1&priorityId=python-priority-agent",
            headers=common,
        )
        self.assertEqual(priority_page.status, 200, priority_page.body[:1000])
        self.assertEqual(
            priority_page.json()["documents"][0]["id"],
            "entity:agent:python-priority-agent",
        )

        tombstone_delta = backend.request(
            "GET",
            "/api/sync/pull?scope=state:entities:v1&after=0&limit=500",
            headers=common,
        )
        self.assertEqual(tombstone_delta.status, 200, tombstone_delta.body[:1000])
        tombstone = next(
            document for document in tombstone_delta.json()["documents"]
            if document["id"] == "entity:agent:python-deleted-agent"
        )
        self.assertTrue(tombstone["_deleted"])
        self.assertEqual(json.loads(tombstone["payload"])["value"], {})

        unknown_cursor = backend.request(
            "GET",
            "/api/sync/pull?scope=state:entities:v1"
            f"&after={max_sequence + 1}&reset=1",
            headers=common,
        )
        self.assertEqual(unknown_cursor.status, 200, unknown_cursor.body[:1000])
        self.assertTrue(unknown_cursor.json()["reset"])
        self.assertEqual(unknown_cursor.json()["maxSeq"], max_sequence)

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
            self.assertEqual(protocol.status, 200, protocol.body[:1000])
            self.assertEqual(sync_state_snapshot(backend.state), before_protocol)

        def entity_preferences_document():
            before = sync_state_snapshot(backend.state) if rust_backend_configured else None
            response = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1"
            )
            if rust_backend_configured:
                self.assertEqual(response.status, 200, response.body[:1000])
                self.assertEqual(sync_state_snapshot(backend.state), before)
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
                "Not implemented: requires transcript-service/pull",
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
        pushed = backend.request(
            "POST", "/api/sync/drafts", body=draft,
            headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
        )
        self.assertEqual(pushed.status, 200, pushed.body[:1000])
        self.assertEqual(pushed.json(), [])
        after_first_draft_write = (
            sync_state_snapshot(backend.state) if rust_backend_configured else None
        )
        if rust_backend_configured:
            replay = backend.request(
                "POST", "/api/sync/drafts", body=draft,
                headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
            )
            self.assertEqual(replay.status, 200, replay.body[:1000])
            self.assertEqual(replay.json(), [])
            self.assertEqual(sync_state_snapshot(backend.state), after_first_draft_write)
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
            after_first_draft_write = sync_state_snapshot(backend.state)
        else:
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
        if rust_backend_configured:
            self.assertEqual(sync_state_snapshot(backend.state), after_first_draft_write)

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
        self.assertEqual(protocol_after_bootstrap.status, 200)
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
        self.assertEqual(draft_after_bootstrap.status, 200, draft_after_bootstrap.body[:1000])
        self.assertEqual(draft_after_bootstrap.json(), [])

if __name__ == "__main__":
    unittest.main(verbosity=2)
