#!/usr/bin/env python3
"""Persisted agent creation, settings, and rename behavior across HTTP backends."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import hashlib
import importlib.util
import json
import os
import sqlite3
import subprocess
import sys
import time
import unittest
import uuid
from contextlib import closing, contextmanager
from pathlib import Path
from urllib.parse import urlencode
from urllib.request import Request, urlopen


isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime

HERE = Path(__file__).resolve().parent
SERVER_SOURCE = HERE.parent / "src"
REVIEW_CORPUS = json.loads((HERE / "fixtures/rust-agents-create/review-corpus.json").read_text())
from http_backend_harness import (
    HTTP_WAIT_GUARD_SECONDS,
    TimedHTTPContractTest,
    start_contract_backend,
    no_write_snapshot as _no_write_snapshot,
)

# These routes can write these application rows. Their dispatcher wakes the
# scheduler only after success; meanwhile it can reserve an earlier queued
# event, so that event's status is treated as background-owned by the harness.
AGENT_CREATE_WRITE_TABLES = frozenset({
    "runtime_agents",
    "runtime_events",
    "runtime_event_meta",
    "runtime_lead_requests",
    "runtime_operation_receipts",
    "runtime_projects",
    "runtime_tasks",
    "sync_entities",
})


def no_write_snapshot(
    state: Path, *, before: tuple[object, ...] | None = None,
) -> tuple[object, ...]:
    return _no_write_snapshot(
        state, include_tables=AGENT_CREATE_WRITE_TABLES, baseline_snapshot=before,
    )


def describe_snapshot_delta(before, after):
    old_databases = dict(before)
    changes = []
    for database, tables in after:
        old_tables = {table: (columns, rows) for table, columns, rows in old_databases.get(database, ())}
        for table, columns, rows in tables:
            old_columns, old_rows = old_tables.get(table, (columns, ()))
            key_columns = tuple(column for column in ("collection", "id") if column in columns)
            if not key_columns and "id" in columns:
                key_columns = ("id",)
            key_indexes = tuple(columns.index(column) for column in key_columns)
            old_by_key = {
                tuple(row[old_columns.index(column)] for column in key_columns): row
                for row in old_rows
            } if key_columns else {row: row for row in old_rows}
            for row in rows:
                key = tuple(row[index] for index in key_indexes) if key_columns else row
                prior = old_by_key.pop(key, None)
                if prior is None:
                    changes.append(f"{database}/{table} added {key!r}")
                    continue
                for index, column in enumerate(columns):
                    if index >= len(old_columns) or column not in old_columns:
                        continue
                    old_value = prior[old_columns.index(column)]
                    if old_value == row[index]:
                        continue
                    if column in {"record", "payload"}:
                        try:
                            old_json, new_json = json.loads(old_value), json.loads(row[index])
                        except (TypeError, ValueError):
                            changes.append(f"{database}/{table}/{key}/{column} changed")
                            continue
                        old_fields = old_json.get("value", old_json) if isinstance(old_json, dict) else old_json
                        new_fields = new_json.get("value", new_json) if isinstance(new_json, dict) else new_json
                        field_names = set(old_fields) | set(new_fields) if isinstance(old_fields, dict) and isinstance(new_fields, dict) else {"<json>"}
                        changed = [name for name in sorted(field_names) if old_fields.get(name) != new_fields.get(name)] if isinstance(old_fields, dict) and isinstance(new_fields, dict) else ["<json>"]
                        changes.append(f"{database}/{table}/{key}/{column} fields={changed[:20]!r}")
                    else:
                        changes.append(f"{database}/{table}/{key}/{column}: {old_value!r} -> {row[index]!r}")
                if len(changes) >= 30:
                    return changes
    return changes

_fixture_spec = importlib.util.spec_from_file_location(
    "agents_create_runtime_fixture", HERE / "runtime-contract.py"
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


@contextmanager
def database_connection(path):
    with closing(sqlite3.connect(path)) as database:
        with database:
            yield database


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
    with database_connection(state / "canvas.sqlite3") as database:
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


def normalized_agent(value):
    result = dict(value)
    result.pop("creationIntent", None)
    own_id = result.get("id")
    result["id"] = "<agent>"
    if result.get("rootId") == own_id:
        result["rootId"] = "<agent>"
    result["created"] = 0
    if "serverId" in result:
        result["serverId"] = "<server>"
    return result


def agent_delta(actual, expected):
    return {
        key: (actual.get(key, "<missing>"), expected.get(key, "<missing>"))
        for key in sorted(actual.keys() | expected.keys())
        if actual.get(key, "<missing>") != expected.get(key, "<missing>")
    }


BACKGROUND_CHILD_COMPLETION_FIELDS = frozenset({
    "status", "inFlight", "startAttempt", "turnId", "turnEpoch",
    "workspaceOperation", "workspaceReservationId", "worktreePreparation",
    "activeTools", "activity", "checkpointError", "checkpointHistoryHead",
    "complaintMisses", "complaintsPresented", "events", "lastAnswer",
    "lastCompletedTurn", "lastCompletedTurnError", "lastCompletedTurnStatus",
    "lastEvent", "nativeRoleGuidance", "nativeTeleportTool", "nativeToolCatalog",
    "nativeToolSource", "prepareAttempt", "preparedContext", "tail",
    "threadId", "worktreeReady", "branch", "cwd",
    # Python's reviewer archive scheduler writes/retries these after delivering
    # child results (codex_agent_management.py:789, 818-826); the quiet draft
    # oracle does not run that scheduler.
    "reviewArchiveAttempts", "reviewArchiveError", "reviewArchiveNextAt",
    "reviewArchiveScheduled",
    # Python's successful thread preparation callback records these even when
    # the provider reply omits them (codex_runtime.py:6477-6478).
    "approvalPolicy", "profile", "sandbox",
})


def python_draft(backend, request, draft=True):
    """Create a draft through the Python runtime so Rust can prove migration replay."""
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    try:
        created = runtime.new_lead(request) if draft else runtime.create(request)
        with runtime.db() as database:
            stored = runtime.agent(created["id"], database)
            projection = json.loads(database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (created["id"],),
            ).fetchone()[0])["value"]
        return {"record": stored, "projection": projection}
    finally:
        runtime.close()


def python_worker_draft(backend, request, parent_id):
    """Capture Python's complete child record, then remove its fixture writes."""
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    child_id = request["id"]
    try:
        created = runtime.create(request, parent=parent_id, defer=False)
        with runtime.db() as database:
            expected = runtime.agent(created["id"], database)
            database.execute("DELETE FROM runtime_agents WHERE id=?", (child_id,))
            database.execute("DELETE FROM sync_entities WHERE collection='agent' AND id=?", (child_id,))
            database.execute("DELETE FROM runtime_events WHERE agent=?", (child_id,))
            database.execute("DELETE FROM runtime_event_meta WHERE id=?", (child_id + ":initial",))
        return expected
    finally:
        runtime.close()


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

    def background_work_pending(self):
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            if database.execute(
                "SELECT 1 FROM runtime_events e JOIN runtime_agents a ON a.id=e.agent "
                "WHERE e.status IN ('pending','reserved','dispatching') "
                "AND e.kind!='child_result' "
                "AND json_extract(a.record,'$.autoWake')=1 "
                "AND json_extract(a.record,'$.status') NOT IN ('failed','interrupted','paused') LIMIT 1"
            ).fetchone():
                return True
            for agent_id, encoded in database.execute("SELECT id,record FROM runtime_agents"):
                agent = json.loads(encoded)
                if agent.get("workspaceOperation") or agent.get("worktreePreparation") in {"waiting", "preparing"}:
                    return True
            for (encoded,) in database.execute("SELECT record FROM runtime_tasks"):
                if json.loads(encoded).get("status") == "running":
                    return True
        return False

    def await_background_quiescence(self):
        """Wait for scheduler, task, and workspace preparation work to settle."""
        deadline = time.monotonic() + HTTP_WAIT_GUARD_SECONDS
        if not self.background_work_pending():
            return
        query = urlencode({
            "protocol": "3",
            "apiSchema": self.backend.schema_hash,
            "resources": json.dumps([{"kind": "state"}], separators=(",", ":")),
        })
        request = Request(
            f"{self.backend.origin}/api/sync/stream?{query}", headers=self.headers(),
        )
        with urlopen(request, timeout=HTTP_WAIT_GUARD_SECONDS) as response:
            raw_socket = response.fp.raw._sock
            while time.monotonic() < deadline:
                if not self.background_work_pending():
                    return
                raw_socket.settimeout(max(0.01, deadline - time.monotonic()))
                event = None
                while time.monotonic() < deadline:
                    line = response.readline().decode("utf-8").rstrip("\r\n")
                    if line.startswith("event: "):
                        event = line.removeprefix("event: ")
                    elif not line and event is not None:
                        if event == "resources":
                            break
                        event = None
            self.fail("Background scheduler/workspace work did not become quiescent")

    def snapshot_no_write(self, *, before=None):
        self.await_background_quiescence()
        snapshot = no_write_snapshot(self.backend.state, before=before)
        if before is not None and snapshot != before:
            self.fail(f"No-write snapshot changed: {describe_snapshot_delta(before, snapshot)!r}")
        return snapshot

    def await_model_catalog(self, account_key):
        backend = self.backend
        query = urlencode({
            "protocol": "3",
            "apiSchema": backend.schema_hash,
            "resources": json.dumps([{"kind": "models"}], separators=(",", ":")),
        })
        request = Request(
            f"{backend.origin}/api/sync/stream?{query}",
            headers=self.headers(),
        )

        def next_resource_event(response, deadline):
            event = None
            raw_socket = response.fp.raw._sock
            while time.monotonic() < deadline:
                raw_socket.settimeout(max(0.01, deadline - time.monotonic()))
                line = response.readline().decode("utf-8").rstrip("\r\n")
                if line.startswith("event: "):
                    event = line.removeprefix("event: ")
                elif not line and event is not None:
                    if event == "resources":
                        return
            raise TimeoutError("Model catalog readiness event did not arrive")

        with urlopen(request, timeout=HTTP_WAIT_GUARD_SECONDS) as response:
            deadline = time.monotonic() + HTTP_WAIT_GUARD_SECONDS
            catalog_query = urlencode({"account_key": account_key})
            while time.monotonic() < deadline:
                result = backend.request(
                    "GET", f"/api/models?{catalog_query}", headers=self.headers(),
                    timeout=max(0.01, deadline - time.monotonic()),
                )
                if result.status == 200:
                    return
                self.assertEqual(result.status, 400, result.body[:1000])
                self.assertTrue(result.json().get("catalogPending"), result.body[:1000])
                next_resource_event(response, deadline)
            self.fail("Model catalog did not become ready before the shared deadline")

    def test_draft_creation_replays_same_content_and_syncs_python_shape(self):
        reference_request = {
            "id": "06c9cf18-bf6e-4d6f-bd38-7a886fb32111",
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
        }
        reference = python_draft(self.backend, reference_request)
        self.backend.start()
        python_replay = self.backend.request(
            "POST", "/api/leads", body=reference_request, headers=self.headers()
        )
        self.assertEqual(python_replay.status, 200, python_replay.body[:1000])
        self.assertEqual(python_replay.json()["id"], reference_request["id"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            python_record = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?",
                (reference_request["id"],),
            ).fetchone()[0])
            python_projection = json.loads(database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (reference_request["id"],),
            ).fetchone()[0])["value"]
        self.assertEqual(normalized_agent(python_record), normalized_agent(reference["record"]))
        normalized_projection = normalized_agent(python_projection)
        expected_projection = normalized_agent(reference["projection"])
        self.assertEqual(
            set(normalized_projection),
            set(expected_projection),
            f"stored-only={set(normalized_projection) - set(expected_projection)}; "
            f"runtime-only={set(expected_projection) - set(normalized_projection)}",
        )
        self.assertEqual(normalized_projection, expected_projection)

        request = {
            "id": "b7f4742c-f61d-4c77-bcfe-51edfc3d17a9",
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
        }
        response = self.backend.request(
            "POST", "/api/leads", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        record = response.json()
        self.assertEqual(record["id"], request["id"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            stored = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
            ).fetchone()
            events = database.execute(
                "SELECT COUNT(*) FROM runtime_events WHERE agent=?", (request["id"],)
            ).fetchone()[0]
            projection = database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (request["id"],),
            ).fetchone()
        self.assertIsNotNone(stored)
        stored_record = json.loads(stored[0])
        self.assertTrue(stored_record["quickCreate"])
        self.assertTrue(stored_record["needsTitle"])
        self.assertEqual(stored_record["cwd"], str(self.backend.cwd))
        actual_record = normalized_agent(stored_record)
        expected_record = normalized_agent(reference["record"])
        self.assertEqual(actual_record, expected_record, agent_delta(actual_record, expected_record))
        self.assertEqual(events, 0)
        self.assertIsNotNone(projection)
        self.assertEqual(normalized_agent(json.loads(projection[0])["value"]), normalized_agent(reference["projection"]))

        replay = self.backend.request(
            "POST", "/api/leads", body=request, headers=self.headers()
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], request["id"])
        changed = {**request, "yolo_mode": False}
        before_conflict = sqlite3.connect(self.backend.state / "canvas.sqlite3")
        try:
            old_record = before_conflict.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
            ).fetchone()
            old_projection = before_conflict.execute(
                "SELECT seq,hash,payload,deleted FROM sync_entities "
                "WHERE collection='agent' AND id=?",
                (request["id"],),
            ).fetchone()
        finally:
            before_conflict.close()
        conflict = self.backend.request(
            "POST", "/api/leads", body=changed, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        after = sqlite3.connect(self.backend.state / "canvas.sqlite3")
        try:
            self.assertEqual(
                after.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
                ).fetchone(),
                old_record,
            )
            self.assertEqual(
                after.execute(
                    "SELECT seq,hash,payload,deleted FROM sync_entities "
                    "WHERE collection='agent' AND id=?",
                    (request["id"],),
                ).fetchone(),
                old_projection,
            )
        finally:
            after.close()

    def test_creation_with_needs_title_succeeds_without_title_port(self):
        request = {
            "id": str(uuid.uuid4()),
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
        }
        response = self.backend.request(
            "POST", "/api/leads", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["id"], request["id"])
        record = self.stored(request["id"])
        self.assertTrue(record["needsTitle"])
        self.assertEqual(record["name"], "New chat")

    def test_persisted_configure_and_exclusions_update_python_written_agent(self):
        configure = self.backend.request(
            "POST",
            "/api/configure",
            body={"id": self.agent_id, "maxAgents": 10, "tokenBudget": 1234},
            headers=self.headers(),
        )
        self.assertEqual(configure.status, 200, configure.body[:1000])
        self.assertEqual(configure.json()["maxAgents"], 10)
        self.assertEqual(configure.json()["tokenBudget"], 1234)
        conversation = self.backend.request(
            "POST",
            "/api/conversation",
            body={"id": self.agent_id, "excluded_account_keys": ["other-account"]},
            headers=self.headers(),
        )
        self.assertEqual(conversation.status, 200, conversation.body[:1000])
        self.assertEqual(conversation.json()["excludedAccountKeys"], ["other-account"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            row = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()
            projection = database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.agent_id,),
            ).fetchone()
        stored = json.loads(row[0])
        self.assertEqual(stored["maxAgents"], 10)
        self.assertEqual(stored["tokenBudget"], 1234)
        self.assertEqual(stored["excludedAccountKeys"], ["other-account"])
        self.assertEqual(
            json.loads(projection[0])["value"]["excludedAccountKeys"],
            ["other-account"],
        )

    def test_p10_delete_commits_without_account_worker(self):
        victim = self.draft()
        victim_id = victim["id"]
        response = self.backend.request(
            "POST",
            "/api/conversation/delete",
            body={"id": victim_id},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        result = response.json()
        self.assertEqual(result["deleted"], [victim_id])
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(result["_syncEntitiesAfter"], result["_syncEntities"][0]["seq"] - 1)
            self.assertEqual(
                result["_syncEntities"][0],
                {
                    "_deleted": True,
                    "id": f"entity:agent:{victim_id}",
                    "payload": json.dumps(
                        {"collection": "agent", "id": victim_id, "value": {}},
                        separators=(",", ":"),
                    ),
                    "seq": result["_syncEntities"][0]["seq"],
                },
            )
        else:
            self.assertEqual(result["_syncEntitiesAfter"], result["_syncEntities"][0]["seq"] - 1)
            self.assertEqual(
                result["_syncEntities"][0],
                {
                    "_deleted": True,
                    "id": f"entity:agent:{victim_id}",
                    "payload": json.dumps(
                        {"collection": "agent", "id": victim_id, "value": {}},
                        separators=(",", ":"),
                    ),
                    "seq": result["_syncEntities"][0]["seq"],
                },
            )
        self.assertTrue(self.stored(victim_id)["deletedAt"])

    def test_rust_agent_creation_writes_one_pending_event_and_replays(self):
        request = {
            "id": "45c18ad9-c7c1-4dba-a65b-986914448b77",
            "cwd": str(self.backend.cwd),
            "prompt": "Persist the initial task before any provider starts.",
            "name": "Persisted lead",
            "workspaceMode": "worktree",
        }
        reference = python_draft(self.backend, {**request, "id": str(uuid.uuid4())}, draft=False)
        self.backend.start()
        response = self.backend.request(
            "POST", "/api/agents", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["id"], request["id"])
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            # The HTTP harness pins provider replay; Python may consume its event immediately.
            with database_connection(self.backend.state / "canvas.sqlite3") as database:
                events = database.execute("SELECT kind,text FROM runtime_events WHERE agent=?", (request["id"],)).fetchall()
            self.assertIn(("user", request["prompt"]), events)
            return
        self.assertEqual(response.json()["status"], "queued")
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            agent = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
            ).fetchone()
            event = database.execute(
                "SELECT kind,text,status FROM runtime_events WHERE id=?",
                (request["id"] + ":initial",),
            ).fetchone()
            projection = database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (request["id"],),
            ).fetchone()
        actual_record = normalized_agent(json.loads(agent[0]))
        expected_record = normalized_agent(reference["record"])
        self.assertEqual(actual_record, expected_record, agent_delta(actual_record, expected_record))
        self.assertEqual(normalized_agent(json.loads(projection[0])["value"]), normalized_agent(reference["projection"]))
        self.assertEqual(
            event,
            ("user", request["prompt"], "pending"),
        )
        self.assertIsNotNone(projection)
        replay = self.backend.request(
            "POST", "/api/agents", body=request, headers=self.headers()
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], request["id"])
        explicit_max = self.backend.request(
            "POST",
            "/api/agents",
            body={**request, "maxAgents": 64},
            headers=self.headers(),
        )
        self.assertEqual(explicit_max.status, 400, explicit_max.body[:1000])
        changed = {**request, "prompt": "Different content must not replace the task."}
        before = (agent, event, projection)
        conflict = self.backend.request(
            "POST", "/api/agents", body=changed, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            after = (
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
                ).fetchone(),
                database.execute(
                    "SELECT kind,text,status FROM runtime_events WHERE id=?",
                    (request["id"] + ":initial",),
                ).fetchone(),
                database.execute(
                    "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                    (request["id"],),
                ).fetchone(),
            )
        self.assertEqual(after, before)

    def test_child_creation_freezes_git_base_and_replays_one_agent_and_event(self):
        self.rust_only()
        subprocess.run(["git", "config", "user.name", "P02 contract"], cwd=self.backend.cwd, check=True)
        subprocess.run(["git", "config", "user.email", "p02@example.test"], cwd=self.backend.cwd, check=True)
        (self.backend.cwd / "README.md").write_text("frozen base\n")
        subprocess.run(["git", "add", "README.md"], cwd=self.backend.cwd, check=True)
        subprocess.run(["git", "commit", "-m", "fixture base"], cwd=self.backend.cwd, check=True, capture_output=True)
        base_commit = subprocess.check_output(
            ["git", "rev-parse", "HEAD^{commit}"], cwd=self.backend.cwd, text=True
        ).strip()
        parent_request = {
            "id": str(uuid.uuid4()),
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
        }
        parent = python_draft(self.backend, parent_request)
        request = {
            "id": str(uuid.uuid4()),
            "parent": parent_request["id"],
            "cwd": str(self.backend.cwd),
            "name": REVIEW_CORPUS["childNameCases"]["trimmedInput"],
            "prompt": "  Use the frozen base.  ",
            "workspaceMode": "worktree",
        }
        expected = python_worker_draft(
            self.backend,
            {
                **{key: value for key, value in request.items() if key != "workspaceMode"},
                "_workerBaseRef": "HEAD",
                "_workerBaseCommit": base_commit,
                "_workspaceMode": "worktree",
                "_workspaceExplicit": True,
            },
            parent["record"]["id"],
        )
        self.backend.start()
        account_key = parent["record"].get("accountKey", "default")
        registry = json.loads((self.backend.state / "accounts/registry.json").read_text())
        account_row = registry["accounts"][account_key]
        self.assertIsInstance(account_row.get("home"), str)
        self.assertIsNone(account_row.get("provider"))
        self.await_model_catalog(account_key)
        before = self.snapshot_no_write()
        first = self.backend.request("POST", "/api/agents", body=request, headers=self.headers())
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertNotEqual(self.snapshot_no_write(), before)
        self.assertEqual(first.json()["id"], request["id"])
        self.backend.stop()
        self.backend.start()
        replay = self.backend.request("POST", "/api/agents", body=request, headers=self.headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], request["id"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            self.assertEqual(
                database.execute("SELECT COUNT(*) FROM runtime_agents WHERE id=?", (request["id"],)).fetchone()[0],
                1,
            )
            self.assertEqual(
                database.execute("SELECT COUNT(*) FROM runtime_events WHERE id=?", (request["id"] + ":initial",)).fetchone()[0],
                1,
            )
            saved = json.loads(database.execute("SELECT record FROM runtime_agents WHERE id=?", (request["id"],)).fetchone()[0])
        self.assertEqual(saved["workerBaseRef"], "HEAD")
        self.assertEqual(saved["workerBaseCommit"], base_commit)
        self.assertTrue(saved["worktree"])
        self.assertEqual(saved["branch"], "codex-agent/" + request["id"])
        self.assertEqual(
            Path(saved["cwd"]),
            self.backend.cwd / ".worktrees" / "codex-agents" / request["id"],
        )
        self.assertEqual(saved["parentId"], parent["record"]["id"])
        self.assertEqual(saved["rootId"], parent["record"]["rootId"])
        expected_fields = REVIEW_CORPUS["childStoredFields"]
        self.assertEqual(saved["name"], expected_fields["name"])
        self.assertEqual(saved["prompt"], expected_fields["prompt"])
        # The oracle draft uses QuietRuntime.schedule(), while the live Rust
        # backend has already scheduled the child. Python Runtime.schedule()
        # also reserves queued workers (codex_runtime.py around 6503/7036),
        # changing only these lifecycle fields after create.
        # The Python draft reference is captured before execution; the live
        # fake-provider child has completed it by this point. These fields are
        # separate from no-write refusal snapshots.
        saved_for_creation = normalized_agent(saved)
        expected_for_creation = normalized_agent(expected)
        for field in BACKGROUND_CHILD_COMPLETION_FIELDS:
            saved_for_creation.pop(field, None)
            expected_for_creation.pop(field, None)
        creation_delta = agent_delta(saved_for_creation, expected_for_creation)
        self.assertEqual(creation_delta, {}, f"field deltas: {creation_delta!r}")

        maximum_name_request = {
            **request,
            "id": str(uuid.uuid4()),
            "name": "x" * REVIEW_CORPUS["childNameCases"]["maximumLength"],
        }
        expected_maximum_name = python_worker_draft(
            self.backend,
            {
                **{key: value for key, value in maximum_name_request.items() if key != "workspaceMode"},
                "_workerBaseRef": "HEAD",
                "_workerBaseCommit": base_commit,
                "_workspaceMode": "worktree",
                "_workspaceExplicit": True,
            },
            parent["record"]["id"],
        )
        self.backend.start()
        self.await_model_catalog(account_key)
        maximum_name_response = self.backend.request(
            "POST", "/api/agents", body=maximum_name_request, headers=self.headers()
        )
        self.assertEqual(maximum_name_response.status, 200, maximum_name_response.body[:1000])
        self.await_background_quiescence()
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            saved_maximum_name = json.loads(
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?",
                    (maximum_name_request["id"],),
                ).fetchone()[0]
            )
        maximum_actual = normalized_agent(saved_maximum_name)
        maximum_expected = normalized_agent(expected_maximum_name)
        for field in BACKGROUND_CHILD_COMPLETION_FIELDS:
            maximum_actual.pop(field, None)
            maximum_expected.pop(field, None)
        self.assertEqual(agent_delta(maximum_actual, maximum_expected), {})

        self.backend.stop()
        self.backend.start()
        replay = self.backend.request("POST", "/api/agents", body=request, headers=self.headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], request["id"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            self.assertEqual(
                database.execute(
                    "SELECT COUNT(*) FROM runtime_agents WHERE id=?",
                    (maximum_name_request["id"],),
                ).fetchone()[0],
                1,
            )
        self.quiesce_fixture_agent(request["id"])
        self.quiesce_fixture_agent(maximum_name_request["id"])

    def test_reviewer_child_uses_shared_workspace_and_read_only_permissions(self):
        parent_request = {
            "id": str(uuid.uuid4()),
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
            "yolo_mode": False,
        }
        parent = python_draft(self.backend, parent_request)
        request = {
            "id": str(uuid.uuid4()),
            "parent": parent["record"]["id"],
            "name": "Reviewer workspace contract",
            "prompt": "Review the shared workspace read-only.",
            "role": "reviewer",
        }
        expected = python_worker_draft(
            self.backend,
            {**request, "_workspaceMode": "shared", "_workspaceExplicit": False},
            parent["record"]["id"],
        )
        self.backend.start()
        account_key = parent["record"].get("accountKey", "default")
        self.await_model_catalog(account_key)
        response = self.backend.request(
            "POST", "/api/agents", body=request, headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.await_background_quiescence()
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            saved = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (request["id"],),
            ).fetchone()[0])
        self.assertEqual(saved["role"], "reviewer")
        self.assertEqual(saved["workspaceMode"], "shared")
        self.assertIs(saved["worktree"], False)
        self.assertIs(saved["yoloMode"], False)
        turn = next(
            frame for frame in reversed(self.backend.received_provider_frames())
            if frame.get("method") == "turn/start"
            and frame.get("params", {}).get("threadId") == saved.get("threadId")
            and frame.get("params", {}).get("input")
        )
        self.assertEqual(turn["params"]["sandboxPolicy"], {"type": "readOnly"})
        saved_for_creation = normalized_agent(saved)
        expected_for_creation = normalized_agent(expected)
        for field in BACKGROUND_CHILD_COMPLETION_FIELDS:
            saved_for_creation.pop(field, None)
            expected_for_creation.pop(field, None)
        self.assertEqual(
            agent_delta(saved_for_creation, expected_for_creation),
            {},
        )

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
        self.assertEqual(len(response_body["_syncEntities"]), 1)
        entity = response_body["_syncEntities"][0]
        self.assertEqual(entity["id"], f"entity:agent:{self.agent_id}")
        self.assertEqual(entity["seq"], response_body["_syncEntitiesAfter"] + 1)
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
        self.assertEqual(response.json()["error"], "Invalid request")
        self.assertIn(response.json()["location"], (["body"], ["body", "request_id"]))
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

    def post(self, path, body, status=200):
        response = self.backend.request("POST", path, body=body, headers=self.headers())
        self.assertEqual(response.status, status, response.body[:1500])
        return response.json()

    def draft(self, **fields):
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "workspaceMode": "worktree", **fields}
        self.post("/api/leads", body)
        return body

    def stored(self, agent_id):
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()[0])

    def projection(self, agent_id):
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            return json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?", (agent_id,)).fetchone()[0])["value"]

    def save(self, record):
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(record), record["id"]))

    def quiesce_fixture_agent(self, agent_id):
        """Make a test-owned queued turn inert before unrelated refusal checks."""
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
            if row is None:
                return
            record = json.loads(row[0])
            record.update(autoWake=False, status="paused", inFlight=False, turnId=None)
            db.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(record), agent_id))

    def rust_only(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Rust strict persisted validation or unavailable service boundary")


    def test_probe_correction_all_creation_values_and_immutable_replay(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "prompt": "Task", "maxAgents": 64}
        self.post("/api/agents", body)
        self.quiesce_fixture_agent(body["id"])
        variations = {**REVIEW_CORPUS["replayChanges"], "cwd": str(self.backend.root), "parent": self.agent_id}
        for field, value in variations.items():
            with self.subTest(field=field):
                before = self.snapshot_no_write()
                self.post("/api/agents", {**body, field: value}, 400)
                self.assertEqual(self.snapshot_no_write(before=before), before)
        self.post("/api/rename", {"id": body["id"], "name": "Changed"})
        self.assertEqual(self.post("/api/agents", body)["name"], "Changed")
        draft = self.draft()
        self.post("/api/rename", {"id": draft["id"], "name": "Changed draft"})
        self.assertEqual(self.post("/api/leads", draft)["name"], "Changed draft")

    def test_probe_admission_orphan_alias_refuses_without_any_write(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "workspaceMode": "worktree"}
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_lead_requests VALUES(?,?,?)", (body["id"], str(uuid.uuid4()), "{}"))
        before = self.snapshot_no_write()
        result = self.post("/api/leads", body, 500)
        self.assertIn("runtime_lead_requests/" + body["id"], result["error"])
        self.assertEqual(self.snapshot_no_write(before=before), before)

    def test_probe_admission_invalid_stored_agent_is_refused_by_rename(self):
        self.rust_only()
        body = self.draft()
        record = self.stored(body["id"])
        try:
            self.save({**record, "cwd": 42})
            before = self.snapshot_no_write()
            result = self.post("/api/rename", {"id": body["id"], "name": "Invalid kept"}, 500)
            self.assertIn("runtime_agents/" + body["id"], result["error"])
            self.assertEqual(self.snapshot_no_write(before=before), before)
        finally:
            self.save(record)

    def test_probe_correction_existing_project_is_validated_with_explicit_account(self):
        self.rust_only()
        self.draft()
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            original = db.execute("SELECT record FROM runtime_projects WHERE id=?", (str(self.backend.cwd),)).fetchone()[0]
            db.execute("UPDATE runtime_projects SET record='{' WHERE id=?", (str(self.backend.cwd),))
        try:
            before = self.snapshot_no_write()
            result = self.post("/api/leads", {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "account_key": "default"}, 500)
            self.assertIn("runtime_projects/" + str(self.backend.cwd), result["error"])
            self.assertEqual(self.snapshot_no_write(before=before), before)
        finally:
            with database_connection(self.backend.state / "canvas.sqlite3") as db:
                db.execute("UPDATE runtime_projects SET record=? WHERE id=?", (original, str(self.backend.cwd)))

    def test_probe_active_exclusions_are_allowed_and_id_only_is_refused(self):
        body = self.draft()
        original = self.stored(body["id"])
        try:
            self.save({**original, "status": "running", "inFlight": True})
            result = self.post("/api/conversation", {"id": body["id"], "excluded_account_keys": []})
            self.assertEqual(result["excludedAccountKeys"], [])
            self.post("/api/conversation", {"id": body["id"]}, 400)
        finally:
            self.save(original)

    def test_probe_correction_native_thread_block_survives_rename(self):
        body = self.draft()
        record = self.stored(body["id"])
        block = {"threadId": "blocked-thread", "error": {"codexErrorInfo": "misalignmentPolicyViolation"}}
        self.save({**record, "threadId": "blocked-thread", "nativeThreadBlock": block})
        self.post("/api/rename", {"id": body["id"], "name": "Still blocked"})
        view = self.projection(body["id"])
        self.assertFalse(view["canSend"])
        self.assertEqual(view["nativeThreadBlock"], block)

    def test_probe_correction_conversation_null_catalog_preferences_are_not_absent(self):
        self.rust_only()
        for field in REVIEW_CORPUS["catalogNulls"]:
            with self.subTest(field=field):
                before = self.snapshot_no_write()
                expected = (
                    400
                    if field in {"model", "fast_mode", "worker_defaults", "review_defaults"}
                    else 200
                    if field in {"daybreak_enabled", "effort"}
                    else 501
                )
                result = self.post("/api/conversation", {"id": self.agent_id, field: None}, expected)
                if expected == 501:
                    self.assertIn("provider-session/model-list", result["error"])
                if field == "effort":
                    self.assertEqual(self.stored(self.agent_id)["nativeEffort"], "medium")
                else:
                    self.assertEqual(self.snapshot_no_write(before=before), before)

    def test_probe_boundaries_explicit_nulls_are_located_schema_refusals(self):
        corpus = [("/api/leads", {"cwd": str(self.backend.cwd)}, ("reuse_empty", "yolo_mode", "cwd", "account_key", "workspaceMode")),
                  ("/api/agents", {"prompt": "Task", "cwd": str(self.backend.cwd)}, ("yolo_mode", "concurrency", "maxAgents", "name", "role", "cwd", "account_key", "workspaceMode")),
                  ("/api/conversation", {"id": self.agent_id}, ("yolo_mode", "excluded_account_keys", "expected_account_key", "cwd", "subagent_concurrency")),
                  ("/api/configure", {"id": self.agent_id}, ("concurrency", "maxAgents"))]
        for path, body, fields in corpus:
            for field in fields:
                for value in (None, {}, []):
                    if field == "excluded_account_keys" and value == []:
                        continue
                    with self.subTest(path=path, field=field, value=value):
                        result = self.post(path, {**body, field: value}, 400)
                        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
                            self.assertIn(field, result["location"])

    def test_probe_boundaries_previous_without_reuse_creates_and_alias_replays(self):
        previous = self.draft(yolo_mode=False)
        body = self.draft(previous=previous["id"], reuse_empty=False)
        self.assertNotEqual(body["id"], previous["id"])
        self.assertFalse(self.stored(body["id"])["yoloMode"])
        alias = str(uuid.uuid4())
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_lead_requests VALUES(?,?,?)", (alias, previous["id"], self.stored(previous["id"])["quickCreateRequest"]))
        self.assertEqual(self.post("/api/leads", {**previous, "id": alias})["id"], previous["id"])
        self.draft(reuse_empty=True)

    def test_probe_boundaries_home_expansion_and_missing_cwd(self):
        self.rust_only()
        (self.backend.home / "project").mkdir(exist_ok=True)
        body = {"id": str(uuid.uuid4()), "prompt": "Task", "cwd": "~/project"}
        result = self.post("/api/agents", body)
        self.assertEqual(result["cwd"], str(self.backend.home / "project"))
        self.post("/api/agents", {"id": str(uuid.uuid4()), "prompt": "Task"}, 400)

    def test_probe_extra_legacy_single_mode_and_deleted_root_keep_tombstones(self):
        body = self.draft()
        original = self.stored(body["id"])
        self.save({**original, "agentMode": "single", "subagentConcurrencyVersion": 1, "concurrency": 32, "deletedAt": 123})
        self.post("/api/configure", {"id": body["id"], "maxAgents": 13})
        saved = self.stored(body["id"])
        self.assertEqual((saved["concurrency"], saved["agentMode"], saved["subagentConcurrencyVersion"]), (0, "single", 2))
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            self.assertEqual(db.execute("SELECT deleted FROM sync_entities WHERE collection='agent' AND id=?", (body["id"],)).fetchone()[0], 1)

    def test_probe_correction_parentless_nonlead_has_no_lead_headroom_or_mode(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "prompt": "Task", "role": "reviewer", "concurrency": 100}
        self.post("/api/agents", body)
        self.assertEqual(self.stored(body["id"])["maxAgents"], 64)
        self.post("/api/configure", {"id": body["id"], "maxAgents": 12})
        saved = self.stored(body["id"])
        for field in ("concurrency", "agentMode", "subagentConcurrencyVersion"):
            self.assertNotIn(field, saved)
        # The live scheduler can reserve the queued worker before this request.
        # Make the Python owner's active-turn precondition explicit rather
        # than depending on scheduler timing (codex_runtime.py:5331, 5353).
        active = self.stored(body["id"])
        active.update(status="running", inFlight=True)
        self.save(active)
        self.post("/api/conversation", {"id": body["id"]}, 400)

    def test_probe_extra_null_creation_effort_and_invalid_stored_name(self):
        self.rust_only()
        for route, body in (("/api/leads", {"cwd": str(self.backend.cwd)}), ("/api/agents", {"cwd": str(self.backend.cwd), "prompt": "Task"})):
            for field in REVIEW_CORPUS["creationCatalogNulls"]:
                agent_id = str(uuid.uuid4())
                if field in REVIEW_CORPUS["creationCatalogNulls"]:
                    self.post(route, {**body, "id": agent_id, field: None}, 200)
                    self.assertIsNotNone(self.stored(agent_id))
                    self.quiesce_fixture_agent(agent_id)
        body = self.draft()
        original = self.stored(body["id"])
        try:
            self.save({**original, "name": 42})
            before = self.snapshot_no_write()
            result = self.post("/api/conversation", {"id": body["id"], "excluded_account_keys": []}, 500)
            self.assertIn("runtime_agents/" + body["id"], result["error"])
            self.assertEqual(self.snapshot_no_write(before=before), before)
        finally:
            self.save(original)

    def test_probe_extra_disconnected_account_replay_does_not_refresh(self):
        self.rust_only()
        body = self.draft()
        self.backend.stop()
        path = self.backend.state / "accounts/registry.json"
        original = path.read_bytes()
        registry = json.loads(original)
        registry["accounts"]["default"]["disconnected"] = True
        path.write_text(json.dumps(registry))
        try:
            self.backend.start()
            before = self.snapshot_no_write()
            self.post("/api/leads", body)
            self.assertEqual(self.snapshot_no_write(before=before), before)
        finally:
            self.backend.stop()
            path.write_bytes(original)
            self.backend.start()

    def test_profiles_are_loaded_validated_and_resolved_from_fresh_catalog(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "prompt": "Task", "profile_id": "unknown-profile"}
        before = self.snapshot_no_write()
        self.post("/api/agents", body, 400)
        self.assertEqual(self.snapshot_no_write(before=before), before)
        profile = {"id": "plain-profile", "name": "Plain", "role": "reviewer", "model": "gpt-6-astra", "effort": None, "instructions": "Follow the fixture instructions."}
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_profiles VALUES(?,?)", (profile["id"], json.dumps(profile)))
        self.post("/api/agents", {**body, "profile_id": profile["id"]}, 200)
        self.assertEqual(self.stored(body["id"])["profileId"], profile["id"])
        self.quiesce_fixture_agent(body["id"])
        for model, status in (("gpt-6-astra", 200), (42, 500)):
            agent_id = str(uuid.uuid4())
            with database_connection(self.backend.state / "canvas.sqlite3") as db:
                db.execute("UPDATE runtime_profiles SET record=? WHERE id=?", (json.dumps({**profile, "model": model}), profile["id"]))
            if status == 200:
                self.post("/api/agents", {**body, "id": agent_id, "profile_id": profile["id"]}, status)
                self.assertEqual(self.stored(agent_id)["model"], model)
                self.quiesce_fixture_agent(agent_id)
            else:
                before = self.snapshot_no_write()
                self.post("/api/agents", {**body, "id": agent_id, "profile_id": profile["id"]}, status)
                self.assertEqual(self.snapshot_no_write(before=before), before)

    def test_probe_admission_completed_worker_result_file_survives_configure(self):
        self.rust_only()
        body = self.draft()
        child = {**self.stored(body["id"]), "id": str(uuid.uuid4()), "rootId": body["id"], "parentId": body["id"], "isLead": False, "status": "completed", "lastCompletedTurn": "turn-x", "lastAnswer": "done"}
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_agents VALUES(?,?)", (child["id"], json.dumps(child)))
            db.execute("INSERT INTO runtime_work VALUES(?,?)", ("result-work", json.dumps({"id": "result-work", "rootId": body["id"], "owner": child["id"], "results": [{"agent": child["id"], "resultFile": "/fixture/result.txt", "created": 1}]})))
        self.post("/api/configure", {"id": body["id"], "maxAgents": 17})
        self.assertEqual(self.projection(child["id"])["overview"]["resultFile"], "/fixture/result.txt")

    def test_every_declared_creation_settings_boundary_refuses_without_writes(self):
        self.rust_only()
        for path, preferences, dependency in REVIEW_CORPUS["boundaries"]:
            with self.subTest(path=path, preferences=preferences):
                body = {"id": self.agent_id} if path in ("/api/configure", "/api/conversation") else {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd)}
                if path == "/api/agents":
                    body["prompt"] = "Task"
                if preferences == {"effort": None}:
                    self.post(path, {**body, **preferences}, 200)
                elif preferences.get("model") == "gpt-6-astra":
                    before = self.snapshot_no_write()
                    response = self.post(path, {**body, **preferences}, 400)
                    self.assertNotIn("Not implemented: requires", response["error"])
                    self.assertEqual(self.snapshot_no_write(before=before), before)
                else:
                    before = self.snapshot_no_write()
                    self.assertEqual(self.post(path, {**body, **preferences}, 501)["error"], "Not implemented: requires " + dependency)
                    self.assertEqual(self.snapshot_no_write(before=before), before)

    def test_probe_refusal_configure_response_failure_rolls_back_every_write(self):
        self.rust_only()
        body = self.draft()
        original = self.stored(body["id"])
        incomplete = dict(original)
        incomplete.pop("maxAgents")
        try:
            self.save(incomplete)
            before = self.snapshot_no_write()
            result = self.post("/api/configure", {"id": body["id"], "tokenBudget": 13}, 500)
            self.assertIn("runtime_agents/" + body["id"], result["error"])
            self.assertEqual(self.snapshot_no_write(before=before), before)
        finally:
            self.save(original)

    def test_probe_new_previous_eligibility_and_nonempty_reuse(self):
        previous = self.draft(yolo_mode=False)
        original = self.stored(previous["id"])
        try:
            for change in ({"deletedAt": 1}, {"isLead": False}):
                with self.subTest(change=change):
                    self.save({**original, **change})
                    before = self.snapshot_no_write()
                    self.post("/api/leads", {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": False}, 400)
                    self.assertEqual(self.snapshot_no_write(before=before), before)
            self.save({**original, "prompt": "This conversation has content"})
            reuse_request = {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": True}
            result = self.post("/api/leads", reuse_request)
            self.assertNotEqual(result["id"], previous["id"])
            self.assertEqual(result["cwd"], original["cwd"])
            self.assertFalse(result["yoloMode"])
            self.draft(previous="")
        finally:
            self.save(original)

    def test_previous_reuse_requires_empty_compatible_workspace_and_provider(self):
        self.rust_only()
        previous = self.draft()
        original = self.stored(previous["id"])
        try:
            reuse_request = {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": True}
            result = self.post("/api/leads", reuse_request)
            self.assertEqual(result["id"], previous["id"])
            with database_connection(self.backend.state / "canvas.sqlite3") as database:
                aliases = database.execute(
                    "SELECT agent FROM runtime_lead_requests WHERE id=?", (reuse_request["id"],)
                ).fetchone()
            self.assertEqual(aliases, (previous["id"],))
            for change in ({"layrReady": True}, {"workspaceMode": "image"}, {"provider": "claude"}, {"threadId": "existing-thread"}):
                with self.subTest(change=change):
                    self.save({**original, **change})
                    result = self.post("/api/leads", {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": True})
                    self.assertNotEqual(result["id"], previous["id"])
        finally:
            self.save(original)

    def test_probe_new_active_exclusions_keep_nullable_control_presence(self):
        body = self.draft()
        original = self.stored(body["id"])
        try:
            self.save({**original, "status": "running", "inFlight": True})
            for field in ("next_turn", "request_id"):
                before = self.snapshot_no_write()
                result = self.post("/api/conversation", {"id": body["id"], "excluded_account_keys": [], field: None}, 400)
                self.assertIn("Wait for this turn", result["error"])
                self.assertEqual(self.snapshot_no_write(before=before), before)
        finally:
            self.save(original)

    def test_probe_new_legacy_replay_explains_missing_intent_without_writes(self):
        self.rust_only()
        original = self.stored(self.agent_id)
        self.assertNotIn("creationIntent", original)
        before = self.snapshot_no_write()
        result = self.post("/api/agents", {"id": self.agent_id, "name": original["name"], "prompt": original["prompt"], "cwd": original["cwd"]}, 400)
        self.assertIn("No saved creation intent", result["error"])
        self.assertIn("inspect the existing agent", result["error"])
        self.assertEqual(self.snapshot_no_write(before=before), before)

    def test_probe_trim_creation_intent_and_rename_use_python_whitespace(self):
        self.rust_only()
        request = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "name": "\u001cName\u001f", "prompt": "\u001dTask\u001e", "workspaceMode": "worktree"}
        reference = python_draft(self.backend, {**request, "id": str(uuid.uuid4())}, draft=False)
        self.backend.start()
        self.post("/api/agents", request)
        record = self.stored(request["id"])
        actual_record = normalized_agent(record)
        expected_record = normalized_agent(reference["record"])
        self.assertEqual(actual_record, expected_record, agent_delta(actual_record, expected_record))
        self.assertEqual(normalized_agent(self.projection(request["id"])), normalized_agent(reference["projection"]))
        self.assertEqual(record["creationIntent"]["name"], "Name")
        self.assertEqual(record["creationIntent"]["prompt"], "Task")
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            self.assertEqual(db.execute("SELECT text FROM runtime_events WHERE agent=?", (request["id"],)).fetchall(), [("Task",)])
        self.post("/api/agents", {**request, "name": "Name", "prompt": "Task"})
        self.quiesce_fixture_agent(request["id"])
        rename = {"id": request["id"], "request_id": "python-trim-rename", "name": "\u001cRenamed\u001f"}
        result = self.post("/api/rename", rename)
        self.assertEqual(result["name"], "Renamed")
        before = self.snapshot_no_write()
        self.assertEqual(self.post("/api/rename", {**rename, "name": "Renamed"})["name"], "Renamed")
        self.assertEqual(self.snapshot_no_write(before=before), before)
        before = self.snapshot_no_write()
        self.post("/api/rename", {"id": request["id"], "name": "\u001c\u001f"}, 400)
        self.assertEqual(self.snapshot_no_write(before=before), before)

    def test_shared_projection_local_server_identity(self):
        body = self.draft()
        self.assertIsInstance(self.projection(body["id"])["serverId"], str)


if __name__ == "__main__":
    unittest.main()
