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

from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot
from sync_http_contract_support import prepare_sync_store_for_rust_contract




class SyncDocumentsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        prepare_sync_store_for_rust_contract(cls.backend)
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            prepare_sync_store_for_rust_contract(cls.backend, seed_python_rows=True)
        cls._prepare_python_entity_tombstone()

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust storage validation")
    def test_sync_and_chat_refuse_agent_missing_epoch_without_projection_writes(self):
        import sqlite3
        from codex_sync_entities import register_functions

        actor={"id":"invalid-epoch-agent","rootId":"invalid-epoch-agent","status":"idle",
               "isLead":True,"name":"Malformed agent","cwd":str(self.backend.cwd)}
        room={"id":"invalid-epoch-room","kind":"private","members":[actor["id"]]}
        with sqlite3.connect(self.backend.state/"canvas.sqlite3") as db:
            register_functions(db)
            for table, row in (("runtime_agents",actor),("runtime_rooms",room)):
                db.execute(f"INSERT INTO {table}(id,record) VALUES(?,?)",(row["id"],json.dumps(row)))
        before=sync_state_snapshot(self.backend.state)
        try:
            for path in ("/api/sync/pull?scope=state:entities:v1&after=0&limit=500&fresh=1&reset=1",
                         "/api/agent-chat?room="+room["id"]):
                response=self.backend.request("GET",path,headers=self._common_headers())
                self.assertEqual(response.status,500,response.body)
                self.assertIn("runtime_agents/"+actor["id"],response.json()["error"])
                self.assertEqual(sync_state_snapshot(self.backend.state),before)
        finally:
            with sqlite3.connect(self.backend.state/"canvas.sqlite3") as db:
                register_functions(db)
                for table,row in (("runtime_rooms",room),("runtime_agents",actor)):
                    db.execute(f"DELETE FROM {table} WHERE id=?",(row["id"],))

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
        worker_id = f"sync-diff-{suffix}-worker"
        turn_error_id = f"sync-diff-{suffix}-turn-error"
        room_id = f"sync-diff-{suffix}-room"
        empty_room_id = f"sync-diff-{suffix}-empty-room"
        complaint_id = f"sync-diff-{suffix}-complaint"
        nullable_complaint_id = f"sync-diff-{suffix}-nullable-complaint"
        project_path = str((backend.cwd / f"sync-diff-{suffix}-project").resolve())
        project_id = project_path
        request_id = f"sync-diff-{suffix}-request"
        rule_id = f"sync-diff-{suffix}-rule"
        work_id = f"sync-diff-{suffix}-work"
        empty_work_id = f"sync-diff-{suffix}-empty-work"
        foreign_work_id = f"sync-diff-{suffix}-foreign-work"
        task_id = f"sync-diff-{suffix}-task"
        monitor_id = f"sync-diff-{suffix}-monitor"
        event_id = f"sync-diff-{suffix}-event"
        chat_id = f"sync-diff-{suffix}-chat"
        edge_id = f"sync-diff-{suffix}-edge"
        spawn_edge_id = f"sync-diff-{suffix}-spawn-edge"
        message_id = f"sync-diff-{suffix}-message"
        identity_path = backend.state / "multi-server" / "identity.json"
        identity_preexisting = identity_path.exists()
        fixtures = {
            "records": {
                "runtime_agents": [
                    [agent_a, {"id": agent_a, "kind": "agent", "isLead": True, "parentId": None,
                     "rootId": agent_a, "epoch": 1, "name": "First lead", "cwd": project_path, "status": "idle",
                     "overview": {"task": "stale lead overview"}}],
                    [agent_b, {"id": agent_b, "kind": "agent", "isLead": True, "parentId": None,
                     "rootId": agent_b, "epoch": 1, "name": None, "cwd": project_path + "/.", "status": "idle"}],
                    [worker_id, {"id": worker_id, "kind": "agent", "isLead": False,
                     "parentId": agent_a, "rootId": agent_a, "epoch": 1, "name": "Worker", "cwd": project_path,
                     "status": "completed", "prompt": "Python view task", "lastAnswer": "done",
                     "lastCompletedTurn": "turn-view", "turnId": "", "threadId": "thread-view",
                     "nativeRelease": {"id": "release-request", "phase": "released",
                                       "resetPending": False, "threadId": "thread-view",
                                       "submittedAt": 10},
                     "nativeStatus": {"phase": "safety", "message": "m" * 3000},
                     "nativeThreadBlock": {"threadId": "thread-view", "error": {"message": "blocked"}}}],
                    [turn_error_id, {"id": turn_error_id, "kind": "agent", "isLead": False,
                     "parentId": agent_a, "rootId": agent_a, "epoch": 1, "name": "Turn error worker",
                     "status": "idle", "threadId": "thread-error", "turnId": "",
                     "nativeTurnError": {"turnId": "", "error": {
                         "codexErrorInfo": "misalignmentPolicyViolation", "message": "blocked"}}}],
                ],
                "runtime_rooms": [[room_id, {"id": room_id, "kind": "private",
                    "members": [agent_a, worker_id], "projectPath": project_path, "updated": 10,
                    "customName": None}],
                    [empty_room_id, {"id": empty_room_id, "kind": "private",
                    "members": [agent_a, worker_id], "projectPath": project_path, "updated": 11,
                    "customName": ""}]],
                "runtime_complaints": [[complaint_id, {"id": complaint_id, "leadId": agent_a,
                    "author": agent_a, "recipient": "user", "status": "open", "text": "A view title",
                    "responses": [], "version": 1, "created": 10, "updated": 10, "readAt": None}],
                    [nullable_complaint_id, {"id": nullable_complaint_id, "leadId": agent_b,
                    "author": agent_b, "recipient": "user", "status": "open", "text": "Nullable names",
                    "responses": [], "version": 1, "created": 11, "updated": 11, "readAt": None}]],
                "runtime_projects": [[project_id, {
                    "id": project_id,
                    "path": project_path,
                    "peerTeams": [{"id": f"sync-diff-{suffix}-team", "name": "Sync test",
                                   "members": [agent_a, agent_b]}],
                    "peerTeamsRevision": 7,
                }]],
                "runtime_requests": [[request_id, {"id": request_id, "status": "pending"}]],
                "runtime_rules": [[rule_id, {"id": rule_id}]],
                "runtime_work": [[work_id, {"id": work_id, "owner": worker_id,
                    "results": [{"agent": worker_id, "resultFile": "results/view.md", "created": 10}]}],
                    [empty_work_id, {"id": empty_work_id, "owner": worker_id,
                    "results": [{"agent": worker_id, "resultFile": "", "created": 20}]}],
                    [foreign_work_id, {"id": foreign_work_id, "owner": agent_a,
                    "results": [{"agent": worker_id, "resultFile": "foreign.md", "created": 30}]}]],
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
            "edges": [[edge_id, agent_a, chat_id, "chat"],
                      [spawn_edge_id, agent_a, worker_id, "spawn"]],
            "messages": [[message_id, chat_id, agent_a, "chat tail", 10, "{}"]],
        }
        fixture_keys = {
            **{table: [row[0] for row in rows] for table, rows in fixtures["records"].items()},
            "runtime_events": [event_id],
            "groups": [chat_id],
            "graph_edges": [edge_id, spawn_edge_id],
            "messages": [message_id],
        }
        identity_setup = f"""
from pathlib import Path
import json, os
identity_path = Path({str(identity_path)!r})
if not identity_path.exists():
    identity_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    identity_path.write_text(json.dumps({{"serverId": "sync-test-server"}}), encoding="utf-8")
    identity_path.chmod(0o600)
local_server_id = json.loads(identity_path.read_text(encoding="utf-8"))["serverId"]
"""
        fixture_json = json.dumps(fixtures, separators=(",", ":"))
        fixture_keys_json = json.dumps(fixture_keys, separators=(",", ":"))
        seed_python = f"""
import json
import os
import sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_canvas import Canvas
from codex_runtime import Runtime
from codex_sync_entities import seed
from codex_peer_teams import sync_entities
canvas = Canvas(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
runtime_owner = object.__new__(Runtime)
runtime_owner.closed = False
{identity_setup}
runtime_owner._project_server_id = local_server_id
runtime_owner.workspace_entity_view = lambda db, base=None: {{
    **(base or {{}}), "stateDir": str(canvas.root), "connected": False,
    "projectOrganizationVersion": 1, "peerTeamsVersion": 1, "tasksHistoryLimit": 100,
}}
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
    seed(db, runtime_owner=runtime_owner, canvas_owner=canvas)
    sync_entities(runtime_owner, db)
    first_with_sequences = rows(db, True)
    first = rows(db, False)
    seed(db, runtime_owner=runtime_owner, canvas_owner=canvas)
    sync_entities(runtime_owner, db)
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
            before_repeat = no_write_snapshot(backend.state)
            repeated = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&limit=500&fresh=1&reset=1",
                headers=headers,
            )
            self.assertEqual(repeated.status, 200, repeated.body[:1000])
            self.assertEqual(no_write_snapshot(backend.state), before_repeat)
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
            if not identity_preexisting:
                identity_path.unlink(missing_ok=True)
                try:
                    identity_path.parent.rmdir()
                except OSError:
                    pass
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
    def test_draft_float_replay_and_invalid_master_refusal(self) -> None:
        import sqlite3

        backend = self.backend
        headers = {
            **self._common_headers(),
            "X-Canvas-Workspace": backend.identity["workspaceId"],
        }
        draft_id = "float-device:float-session"
        first = {
            "device": "float-device", "session": "float-session",
            "text": "first", "extension": {"small": 1e-07},
        }
        write = lambda payload, **extra: backend.request(
            "POST", "/api/sync/drafts",
            body={"rows": [{"newDocumentState": {
                "id": draft_id, "payload": json.dumps(payload),
                **extra,
            }}]},
            headers=headers,
        )
        created = write(first)
        self.assertEqual(created.status, 200, created.body[:1000])
        before_replay = no_write_snapshot(backend.state)
        replay = write(first)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json(), [])
        self.assertEqual(no_write_snapshot(backend.state), before_replay)

        updated = {**first, "text": "updated"}
        optimistic = backend.request(
            "POST", "/api/sync/drafts",
            body={"rows": [{
                "newDocumentState": {"id": draft_id, "payload": json.dumps(updated)},
                "assumedMasterState": {"id": draft_id, "payload": json.dumps(first)},
            }]},
            headers=headers,
        )
        self.assertEqual(optimistic.status, 200, optimistic.body[:1000])
        self.assertEqual(optimistic.json(), [])

        invalid_number = backend.request(
            "POST", "/api/sync/drafts",
            body={"rows": [{"newDocumentState": {
                "id": "large-device:large-session",
                "payload": '{"device":"large-device","session":"large-session",'
                           '"extra":{"n":18446744073709551617}}',
            }}]},
            headers=headers,
        )
        self.assertEqual(invalid_number.status, 400, invalid_number.body[:1000])
        self.assertEqual(invalid_number.json()["error"], "Invalid request")
        self.assertTrue(invalid_number.json().get("location"))

        invalid_null_id = backend.request(
            "POST", "/api/sync/drafts",
            body={"rows": [{"newDocumentState": {
                "id": "null-device:null-session",
                "payload": json.dumps({"device": "null-device", "session": "null-session", "id": None}),
            }}]},
            headers=headers,
        )
        self.assertEqual(invalid_null_id.status, 400, invalid_null_id.body[:1000])

        invalid_master_id = "invalid-master-device:invalid-master-session"
        database_path = backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            database.execute(
                "INSERT INTO sync_documents(scope,id,payload,deleted) VALUES('drafts',?,?,0)",
                (invalid_master_id, "not-json"),
            )
        try:
            before_refusal = no_write_snapshot(backend.state)
            refusal = backend.request(
                "POST", "/api/sync/drafts",
                body={"rows": [
                    {"newDocumentState": {
                        "id": "batch-device:valid-session",
                        "payload": json.dumps({
                            "device": "batch-device", "session": "valid-session", "text": "batch",
                        }),
                    }},
                    {"newDocumentState": {
                        "id": invalid_master_id,
                        "payload": json.dumps({
                            "device": "invalid-master-device",
                            "session": "invalid-master-session",
                            "text": "replacement",
                        }),
                    }},
                ]},
                headers=headers,
            )
            self.assertEqual(refusal.status, 500, refusal.body[:1000])
            self.assertIn(f"drafts/{invalid_master_id}", refusal.json()["error"])
            self.assertEqual(no_write_snapshot(backend.state), before_refusal)
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute(
                    "DELETE FROM sync_documents WHERE scope='drafts' AND id=?", (invalid_master_id,),
                )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_entity_refusals_roll_back_projection_maintenance(self) -> None:
        import sqlite3

        backend = self.backend
        database_path = backend.state / "canvas.sqlite3"
        headers = self._common_headers()
        with sqlite3.connect(database_path) as database:
            original_preference = database.execute(
                "SELECT payload FROM sync_entities WHERE collection='uiPreferences' AND id='user'"
            ).fetchone()[0]
            database.execute(
                "INSERT INTO runtime_rules(id,record) VALUES('sync-pull-rollback-rule',?)",
                (json.dumps({"id": "sync-pull-rollback-rule"}),),
            )
            database.execute(
                "UPDATE sync_entities SET payload=? WHERE collection='uiPreferences' AND id='user'",
                ('{"collection":"uiPreferences","id":"user",'
                 '"value":{"fields":{},"unexpectedSecret":true}}',),
            )
        before = no_write_snapshot(backend.state)
        try:
            response = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&limit=500&fresh=1&reset=1",
                headers=headers,
            )
            self.assertEqual(response.status, 500, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                "Invalid stored sync object uiPreferences/user",
            )
            self.assertEqual(no_write_snapshot(backend.state), before)
            with sqlite3.connect(database_path) as database:
                self.assertIsNone(database.execute(
                    "SELECT 1 FROM sync_entities WHERE collection='rule' AND id='sync-pull-rollback-rule'"
                ).fetchone())
                database.execute(
                    "UPDATE sync_entities SET payload=? WHERE collection='uiPreferences' AND id='user'",
                    (original_preference,),
                )
                database.execute("DELETE FROM runtime_rules WHERE id='sync-pull-rollback-rule'")
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute(
                    "UPDATE sync_entities SET payload=? WHERE collection='uiPreferences' AND id='user'",
                    (original_preference,),
                )
                database.execute("DELETE FROM runtime_rules WHERE id='sync-pull-rollback-rule'")
                database.execute(
                    "DELETE FROM sync_entities WHERE collection='rule' AND id='sync-pull-rollback-rule'"
                )

        broken_id = "sync-pull-broken-record"
        with sqlite3.connect(database_path) as database:
            database.execute("INSERT INTO runtime_rules(id,record) VALUES(?, '[]')", (broken_id,))
        before_broken = no_write_snapshot(backend.state)
        try:
            broken = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&limit=500&fresh=1&reset=1",
                headers=headers,
            )
            self.assertEqual(broken.status, 500, broken.body[:1000])
            self.assertIn(f"runtime_rules/{broken_id}", broken.json()["error"])
            self.assertEqual(no_write_snapshot(backend.state), before_broken)
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute("DELETE FROM runtime_rules WHERE id=?", (broken_id,))

        invalid_projection_id = "sync-pull-invalid-unreturned-agent"
        with sqlite3.connect(database_path) as database:
            database.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?,?)",
                (invalid_projection_id, json.dumps({
                    "id": invalid_projection_id, "rootId": invalid_projection_id, "epoch": 1, "status": "idle",
                    "name": "Invalid native release",
                    "nativeRelease": {"phase": [], "resetPending": False},
                })),
            )
        before_invalid_projection = no_write_snapshot(backend.state)
        try:
            invalid_projection = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&limit=1",
                headers=headers,
            )
            self.assertEqual(invalid_projection.status, 500, invalid_projection.body[:1000])
            self.assertIn(
                f"agent/{invalid_projection_id}",
                invalid_projection.json()["error"],
            )
            self.assertEqual(no_write_snapshot(backend.state), before_invalid_projection)
            with sqlite3.connect(database_path) as database:
                self.assertIsNone(database.execute(
                    "SELECT 1 FROM sync_entities WHERE collection='agent' AND id=?",
                    (invalid_projection_id,),
                ).fetchone())
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute("DELETE FROM runtime_agents WHERE id=?", (invalid_projection_id,))

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_monitor_window_is_selected_before_owner_filter(self) -> None:
        import sqlite3

        backend = self.backend
        database_path = backend.state / "canvas.sqlite3"
        deleted_owner = "sync-window-deleted-owner"
        live_owner = "sync-window-live-owner"
        old_monitor = "sync-window-old-live-monitor"
        rows = [
            (deleted_owner, json.dumps({"id": deleted_owner, "rootId": deleted_owner, "epoch": 1, "status": "idle", "deletedAt": 1})),
            (live_owner, json.dumps({"id": live_owner, "rootId": live_owner, "epoch": 1, "status": "idle"})),
        ]
        rows.extend(
            (f"sync-window-terminal-{index}", json.dumps({
                "id": f"sync-window-terminal-{index}", "agent": deleted_owner,
                "status": "completed", "created": 10_000 - index,
            }))
            for index in range(100)
        )
        rows.append((old_monitor, json.dumps({
            "id": old_monitor, "agent": live_owner, "status": "completed", "created": 1,
        })))
        with sqlite3.connect(database_path) as database:
            database.executemany(
                "INSERT OR REPLACE INTO runtime_agents(id,record) VALUES(?,?)", rows[:2],
            )
            database.executemany(
                "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES(?,?)", rows[2:],
            )
        response = backend.request(
            "GET", "/api/sync/pull?scope=state:entities:v1&after=0&limit=500&fresh=1&reset=1",
            headers=self._common_headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        with sqlite3.connect(database_path) as database:
            self.assertIsNone(database.execute(
                "SELECT 1 FROM sync_entities WHERE collection='monitor' AND id=?", (old_monitor,),
            ).fetchone())
            database.executemany("DELETE FROM runtime_agents WHERE id=?", [(deleted_owner,), (live_owner,)])
            database.executemany("DELETE FROM runtime_monitors WHERE id=?", [(row[0],) for row in rows[2:]])
            database.executemany("DELETE FROM sync_entities WHERE collection='monitor' AND id=?", [(row[0],) for row in rows[2:]])
            database.executemany("DELETE FROM sync_entities WHERE collection='agent' AND id=?", [(deleted_owner,), (live_owner,)])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_nested_draft_payload_rejects_entire_batch_without_writes(self) -> None:
        backend = self.backend
        before = no_write_snapshot(backend.state)
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
        self.assertEqual(no_write_snapshot(backend.state), before)

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
        before = no_write_snapshot(backend.state)
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
        self.assertEqual(no_write_snapshot(backend.state), before)
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
                before = no_write_snapshot(backend.state)
                response = backend.request(
                    "GET", "/api/sync/pull?scope=drafts&after=0", headers=headers,
                )
                self.assertEqual(response.status, 500, response.body[:1000])
                self.assertEqual(
                    response.json()["error"],
                    f"Invalid stored sync object drafts/{draft_id}",
                )
                self.assertEqual(no_write_snapshot(backend.state), before)
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

            before_overflowing_cursor = no_write_snapshot(backend.state)
            overflowing_cursor = backend.request(
                "GET", "/api/sync/pull?scope=drafts&after=9223372036854775808",
                headers=common,
            )
            self.assertEqual(overflowing_cursor.status, 400, overflowing_cursor.body[:1000])
            self.assertEqual(overflowing_cursor.json()["error"], "Invalid request")
            self.assertEqual(no_write_snapshot(backend.state), before_overflowing_cursor)

            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute("UPDATE sync_generation SET value=5 WHERE id=1")
            before_protocol = no_write_snapshot(backend.state)
            protocol = backend.request("GET", "/api/sync/protocol", headers=common)
            self.assertEqual(protocol.status, 200, protocol.body[:1000])
            self.assertEqual(no_write_snapshot(backend.state), before_protocol)

        def entity_preferences_document():
            before = no_write_snapshot(backend.state) if rust_backend_configured else None
            response = backend.request(
                "GET", "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1"
            )
            if rust_backend_configured:
                self.assertEqual(response.status, 200, response.body[:1000])
                self.assertEqual(no_write_snapshot(backend.state), before)
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
            no_write_snapshot(backend.state) if rust_backend_configured else None
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
            self.assertEqual(no_write_snapshot(backend.state), before_stable_replay)
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
            before_invalid_preference = no_write_snapshot(backend.state)
            invalid_stored_preference = backend.request(
                "POST", "/api/sync/preferences", body={"scope": "user", "fields": {}},
                headers=common,
            )
            self.assertEqual(invalid_stored_preference.status, 500)
            self.assertIn("uiPreferences/user", invalid_stored_preference.json()["error"])
            self.assertEqual(no_write_snapshot(backend.state), before_invalid_preference)
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
        before_invalid_scope_payload = no_write_snapshot(backend.state)
        invalid_scope_payload = backend.request(
            "POST", "/api/sync/preferences",
            body={"scope": "user", "fields": {
                "key": {"timestamp": 1, "value": {"key": {"key": {"key": None}}}, "writer": "x"},
            }},
            headers=common,
        )
        self.assertEqual(invalid_scope_payload.status, 400)
        self.assertEqual(no_write_snapshot(backend.state), before_invalid_scope_payload)
        before_wrong_scope = no_write_snapshot(backend.state)
        wrong_scope = backend.request(
            "POST", "/api/sync/preferences",
            body={"scope": "server", "fields": {
                theme_key: {"value": "dark", "timestamp": 30, "writer": "phone-a"},
            }},
            headers=common,
        )
        self.assertEqual(wrong_scope.status, 400)
        self.assertEqual(no_write_snapshot(backend.state), before_wrong_scope)
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

        transcript = backend.request("GET", "/api/sync/pull?scope=transcript:agent-a")
        self.assertEqual(transcript.status, 200, transcript.body[:1000])
        self.assertIn("documents", transcript.json())

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
            no_write_snapshot(backend.state) if rust_backend_configured else None
        )
        if rust_backend_configured:
            replay = backend.request(
                "POST", "/api/sync/drafts", body=draft,
                headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
            )
            self.assertEqual(replay.status, 200, replay.body[:1000])
            self.assertEqual(replay.json(), [])
            self.assertEqual(no_write_snapshot(backend.state), after_first_draft_write)
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute(
                    "INSERT OR REPLACE INTO sync_documents(scope,id,payload,deleted) "
                    "VALUES ('drafts','bad-json','not-json',0)"
                )
            before_invalid_draft = no_write_snapshot(backend.state)
            invalid_stored_draft = backend.request(
                "GET", "/api/sync/pull?scope=drafts&after=0", headers=common,
            )
            self.assertEqual(invalid_stored_draft.status, 500)
            self.assertIn("drafts/bad-json", invalid_stored_draft.json()["error"])
            self.assertNotIn("not-json", invalid_stored_draft.json()["error"])
            self.assertEqual(no_write_snapshot(backend.state), before_invalid_draft)
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute(
                    "DELETE FROM sync_documents WHERE scope='drafts' AND id='bad-json'"
                )
            after_first_draft_write = no_write_snapshot(backend.state)
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
        before_invalid_identity = no_write_snapshot(backend.state)
        invalid_draft = backend.request(
            "POST", "/api/sync/drafts", body=invalid_identity,
            headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
        )
        self.assertEqual(invalid_draft.status, 400)
        self.assertEqual(no_write_snapshot(backend.state), before_invalid_identity)
        invalid_payload = {
            "rows": [{"newDocumentState": {"id": "x", "payload": "x"}} for _ in range(99)],
        }
        before_invalid_payload = no_write_snapshot(backend.state)
        invalid_serialized_payload = backend.request(
            "POST", "/api/sync/drafts", body=invalid_payload,
            headers={**common, "X-Canvas-Workspace": backend.identity["workspaceId"]},
        )
        self.assertEqual(invalid_serialized_payload.status, 400)
        self.assertEqual(no_write_snapshot(backend.state), before_invalid_payload)

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
