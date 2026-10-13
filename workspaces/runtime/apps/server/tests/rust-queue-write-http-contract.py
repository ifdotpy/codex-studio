#!/usr/bin/env python3
"""Queue writes through the HTTP harness against Python and Rust backends."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import base64
import hashlib
import importlib.util
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import unittest
import uuid
from contextlib import closing
from pathlib import Path


isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "src"))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot

fixture_spec = importlib.util.spec_from_file_location("queue_write_fixture", HERE / "runtime-contract.py")
fixture = importlib.util.module_from_spec(fixture_spec)
assert fixture_spec.loader is not None
fixture_spec.loader.exec_module(fixture)


def seed_queue(backend: object) -> tuple[str, list[str]]:
    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = make_runtime(QuietRuntime, backend, fixture.FakeServer)
    try:
        agent = runtime.create({
            "name": "Queue write contract",
            "cwd": str(backend.cwd),
            "prompt": "Seed Python-written queue events for the HTTP contract.",
            "autoWake": False,
        }, defer=True)
        agent.update(autoWake=True, status="idle")
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
        asset = runtime.upload_asset({
            "agent": agent["id"], "name": "queue-attachment.txt",
            "base64": base64.b64encode(b"queue attachment").decode(),
        })
        queue_ids = [f"queue-{uuid.uuid4().hex[:8]}-{index}" for index in range(3)]
        with runtime.db() as database:
            for index in range(3):
                runtime.enqueue(database, agent, "user", f"Message {index}", queue_ids[index])
            metadata = json.loads(database.execute(
                "SELECT record FROM runtime_event_meta WHERE id=?", (queue_ids[0],)
            ).fetchone()[0])
            metadata["assets"] = [asset["id"]]
            database.execute(
                "UPDATE runtime_event_meta SET record=? WHERE id=?",
                (json.dumps(metadata), queue_ids[0]),
            )
            agent.update(autoWake=False, status="idle")
            runtime.put(database, "agents", agent)
        return agent["id"], queue_ids
    finally:
        runtime.close()




def clone_for_python(backend: object) -> Path:
    root = Path(tempfile.mkdtemp(prefix="queue-reference-", dir=os.environ["TMPDIR"]))
    with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as source:
        with closing(sqlite3.connect(root / "canvas.sqlite3")) as target:
            source.backup(target)
    return root


def normalized_state(state: Path):
    normalized = {}
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        tables = [row[0] for row in database.execute(
            "SELECT name FROM sqlite_master WHERE type='table' "
            "AND (name LIKE 'runtime_%' OR name LIKE 'sync_%') ORDER BY name"
        )]
        # Python queue_action advances these scheduler-only generations while
        # the Rust queue contract leaves agent records alone. Compare durable
        # queue rows, event metadata, receipts, and sync projections instead.
        scheduler_tables = {
            "runtime_agent_record_generation",
            "runtime_scheduler_agent_change_state",
            "runtime_scheduler_agent_changes",
        }
        tables = [table for table in tables if table not in scheduler_tables]
        for table in tables:
            columns = [row[1] for row in database.execute(f'PRAGMA table_info("{table}")')]
            rows = []
            for values in database.execute(f'SELECT * FROM "{table}"'):
                row = {}
                for column, value in zip(columns, values):
                    if column in {"sequence", "seq", "revision", "signature", "hash", "created", "acceptedAt", "updatedAt"}:
                        value = "<opaque>"
                    elif column in {"record", "payload", "result"} and isinstance(value, str):
                        try:
                            value = json.loads(value)
                        except json.JSONDecodeError:
                            pass
                    row[column] = normalize_value(value)
                rows.append(row)
            normalized[table] = sorted(rows, key=repr)
    return normalized


def normalize_value(value, key=None):
    if isinstance(value, dict):
        return {
            field: normalize_value(item, field)
            for field, item in value.items()
            if field not in {"revision", "sequence", "seq", "acceptedAt", "created", "updatedAt", "serverId"}
        }
    if isinstance(value, list):
        return [normalize_value(item) for item in value]
    return value


def state_difference(actual, expected):
    for table in sorted(set(actual) | set(expected)):
        if actual.get(table) == expected.get(table):
            continue
        actual_rows = {repr(row.get("id")): row for row in actual.get(table, [])}
        expected_rows = {repr(row.get("id")): row for row in expected.get(table, [])}
        for identity in sorted(set(actual_rows) | set(expected_rows)):
            left = actual_rows.get(identity)
            right = expected_rows.get(identity)
            if left != right:
                if left is None or right is None:
                    return f"{table}/{identity}: row presence differs"
                fields = [key for key in sorted(set(left) | set(right)) if left.get(key) != right.get(key)]
                if fields == ["payload"] and isinstance(left.get("payload"), dict) and isinstance(right.get("payload"), dict):
                    left_value = left["payload"].get("value", {})
                    right_value = right["payload"].get("value", {})
                    payload_fields = [key for key in sorted(set(left_value) | set(right_value)) if left_value.get(key) != right_value.get(key)]
                    return f"{table}/{identity}: payload.value differs at {payload_fields}; " + "; ".join(
                        f"{key}={str(left_value.get(key))[:160]!r}/{str(right_value.get(key))[:160]!r}"
                        for key in payload_fields
                    )
                return f"{table}/{identity}: differing fields {fields}; " + "; ".join(
                    f"{key}={str(left.get(key))[:120]!r}/{str(right.get(key))[:120]!r}" for key in fields
                )
        return f"{table}: row sets differ"
    return ""


def python_mutation(backend: object, agent_id: str, body: dict):
    root = clone_for_python(backend)

    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = make_runtime(QuietRuntime, root, fixture.FakeServer)
    try:
        python_body = dict(body)
        if "expected_revision" in python_body:
            python_body["expected_revision"] = runtime.queue_action(agent_id)["revision"]
        result = runtime.queue_action(agent_id, python_body)
        return result, normalized_state(root)
    finally:
        runtime.close()


class QueueWriteHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        fixture_counts = {
            "test_each_action_matches_python_rows_and_receipt_replay_is_read_only": 5,
            "test_failure_after_first_write_rolls_back_every_table": 1,
            "test_malformed_stored_records_name_their_table_and_id": 1,
            "test_original_text_length_limit_and_python_strip_boundaries": 1,
            "test_reads_python_written_queue_and_applies_durable_mutations": 1,
            "test_reorder_ties_remain_before_the_system_event_boundary": 1,
            "test_stale_and_invalid_mutations_leave_state_unchanged": 1,
            "test_z_first_and_send_now_move_the_requested_message": 1,
        }
        cls.fixtures = {
            method: [seed_queue(cls.backend) for _ in range(count)]
            for method, count in fixture_counts.items()
        }
        cls.backend.start()

    def setUp(self) -> None:
        super().setUp()
        self.fixture_sets = self.fixtures[self._testMethodName]
        self.agent_id, self.queue_ids = self.fixture_sets[0]

    def headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def request(self, body):
        return self.backend.request("POST", "/api/queue", body=body, headers=self.headers())

    def get_queue(self):
        return self.backend.request("GET", f"/api/queue?agent={self.agent_id}", headers=self.headers())

    def test_each_action_matches_python_rows_and_receipt_replay_is_read_only(self):
        actions = [
            ("cancel", {"index": 1}),
            ("edit", {"index": 1, "expectedText": "Message 1", "text": "Edited \x7f text"}),
            ("first", {"index": 2}),
            ("send_now", {"index": 2}),
            ("reorder", {"order": [2, 1, 0]}),
        ]
        for fixture_index, (action, fields) in enumerate(actions):
            with self.subTest(action=action):
                self.agent_id, self.queue_ids = self.fixture_sets[fixture_index]
                current = self.get_queue()
                self.assertEqual(current.status, 200, current.body[:1000])
                fields = dict(fields)
                if "index" in fields:
                    fields["message_id"] = self.queue_ids[fields.pop("index")]
                if "order" in fields:
                    fields["ordered_ids"] = [self.queue_ids[index] for index in fields.pop("order")]
                body = {
                    "agent": self.agent_id,
                    "action": action,
                    "request_id": f"parity-{action}",
                    **fields,
                }
                if action == "reorder":
                    body["expected_revision"] = current.json()["revision"]
                python_result, python_state = python_mutation(self.backend, self.agent_id, body)
                actual = self.request(body)
                self.assertEqual(actual.status, 200, actual.body[:1000])
                self.assertEqual(actual.json()["status"], python_result["status"])
                self.assertEqual(actual.json()["capabilities"], python_result["capabilities"])
                actual_state = normalized_state(self.backend.state)
                differences = [
                    table for table in sorted(set(actual_state) | set(python_state))
                    if actual_state.get(table) != python_state.get(table)
                ]
                self.assertEqual(differences, [], f"persisted queue rows differ: {state_difference(actual_state, python_state)}")

                before_replay = no_write_snapshot(self.backend.state)
                replay = self.request(body)
                self.assertEqual(replay.status, 200, replay.body[:1000])
                self.assertEqual(replay.json()["status"], actual.json()["status"])
                self.assertEqual(replay.json()["revision"], actual.json()["revision"])
                self.assertEqual(replay.json()["capabilities"], actual.json()["capabilities"])
                with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
                    saved = json.loads(database.execute(
                        "SELECT result FROM runtime_operation_receipts WHERE id=?",
                        (f"queue:{self.agent_id}:parity-{action}",),
                    ).fetchone()[0])
                self.assertEqual(
                    {key: replay.json()[key] for key in ("status", "revision", "capabilities")},
                    saved,
                )
                self.assertEqual(no_write_snapshot(self.backend.state), before_replay)

                if action == "edit":
                    conflict = self.request({**body, "text": "Conflicting content"})
                    self.assertEqual(conflict.status, 400, conflict.body[:1000])
                    self.assertEqual(no_write_snapshot(self.backend.state), before_replay)

    def test_reads_python_written_queue_and_applies_durable_mutations(self):
        initial = self.get_queue()
        self.assertEqual(initial.status, 200, initial.body[:1000])
        self.assertEqual([item["id"] for item in initial.json()["items"]], self.queue_ids)
        self.assertEqual(initial.json()["items"][0]["text"], "Message 0")
        self.assertEqual(len(initial.json()["items"][0]["assets"]), 1)
        self.assertNotIn("path", initial.json()["items"][0]["assets"][0])

        edit = {
            "agent": self.agent_id, "action": "edit", "request_id": "edit-1",
            "message_id": self.queue_ids[0], "expected_revision": initial.json()["revision"],
            "expectedText": "Message 0", "text": "Edited message",
        }
        edited = self.request(edit)
        self.assertEqual(edited.status, 200, edited.body[:1000])
        self.assertEqual(edited.json()["status"], "updated")
        self.assertEqual(self.request(edit).json(), edited.json())
        self.assertEqual(self.get_queue().json()["items"][0]["text"], "Edited message")

        current = self.get_queue().json()
        reorder = {
            "agent": self.agent_id, "action": "reorder", "request_id": "reorder-1",
            "expected_revision": current["revision"],
            "ordered_ids": [self.queue_ids[2], self.queue_ids[1], self.queue_ids[0]],
        }
        response = self.request(reorder)
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual([item["id"] for item in self.get_queue().json()["items"]], reorder["ordered_ids"])

        current = self.get_queue().json()
        cancel = {
            "agent": self.agent_id, "action": "cancel", "request_id": "cancel-1",
            "expected_revision": current["revision"], "message_id": self.queue_ids[1],
        }
        self.assertEqual(self.request(cancel).status, 200)
        self.assertNotIn(self.queue_ids[1], [item["id"] for item in self.get_queue().json()["items"]])

    def test_stale_and_invalid_mutations_leave_state_unchanged(self):
        initial = self.get_queue().json()
        before = no_write_snapshot(self.backend.state)
        stale = self.request({
            "agent": self.agent_id, "action": "reorder", "request_id": "stale-1",
            "expected_revision": "stale-revision", "ordered_ids": list(reversed(self.queue_ids)),
        })
        self.assertEqual(stale.status, 400, stale.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        invalid = self.request({
            "agent": self.agent_id, "action": "send_now", "message_id": self.queue_ids[0],
        })
        self.assertEqual(invalid.status, 400, invalid.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        blank_identity = self.request({
            "agent": self.agent_id, "action": "send_now", "request_id": "  ",
            "message_id": self.queue_ids[0],
        })
        self.assertEqual(blank_identity.status, 400, blank_identity.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        self.assertEqual(self.get_queue().json(), initial)

    def test_failure_after_first_write_rolls_back_every_table(self):
        current = self.get_queue().json()
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute(
                "CREATE TRIGGER fail_queue_metadata BEFORE INSERT ON runtime_event_meta "
                "BEGIN SELECT RAISE(ABORT, 'injected queue metadata failure'); END"
            )
            database.commit()
        before = no_write_snapshot(self.backend.state)
        try:
            response = self.request({
                "agent": self.agent_id, "action": "edit", "request_id": "rollback-after-write",
                "expected_revision": current["revision"], "message_id": self.queue_ids[0],
                "expectedText": "Message 0", "text": "This first SQL write must roll back",
            })
            self.assertNotEqual(response.status, 200, response.body[:1000])
        finally:
            with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
                database.execute("DROP TRIGGER IF EXISTS fail_queue_metadata")
                database.commit()
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_original_text_length_limit_and_python_strip_boundaries(self):
        current = self.get_queue().json()
        before = no_write_snapshot(self.backend.state)
        too_long = self.request({
            "agent": self.agent_id, "action": "edit", "message_id": self.queue_ids[0],
            "text": " " + "x" * 32000,
        })
        self.assertEqual(too_long.status, 400, too_long.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

        request_id = "\x1c strip-key \x1f"
        body = {
            "agent": self.agent_id, "action": "edit", "request_id": request_id,
            "expected_revision": current["revision"], "message_id": self.queue_ids[0],
            "expectedText": "Message 0", "text": "\x1c Updated text \x1f",
        }
        python_result, python_state = python_mutation(self.backend, self.agent_id, body)
        actual = self.request(body)
        self.assertEqual(actual.status, 200, actual.body[:1000])
        self.assertEqual(actual.json()["status"], python_result["status"])
        self.assertEqual(normalized_state(self.backend.state), python_state)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            receipt = database.execute(
                "SELECT id FROM runtime_operation_receipts WHERE id=?",
                (f"queue:{self.agent_id}:strip-key",),
            ).fetchone()
            self.assertIsNotNone(receipt)

    def test_malformed_stored_records_name_their_table_and_id(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Rust stored-object diagnostics are exercised on the Rust backend")
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("PRAGMA ignore_check_constraints=ON")
            original_metadata = database.execute(
                "SELECT record FROM runtime_event_meta WHERE id=?", (self.queue_ids[0],)
            ).fetchone()[0]
            triggers = database.execute(
                "SELECT name,sql FROM sqlite_master WHERE type='trigger' AND tbl_name='runtime_event_meta'"
            ).fetchall()
            indexes = database.execute(
                "SELECT name,sql FROM sqlite_master WHERE type='index' AND tbl_name='runtime_event_meta' AND sql IS NOT NULL"
            ).fetchall()
            for name, _sql in triggers:
                database.execute(f'DROP TRIGGER "{name}"')
            for name, _sql in indexes:
                database.execute(f'DROP INDEX "{name}"')
            database.execute(
                "UPDATE runtime_event_meta SET record='{' WHERE id=?", (self.queue_ids[0],)
            )
            database.commit()
        try:
            event = self.get_queue()
            self.assertEqual(event.status, 500, event.body[:1000])
            self.assertIn(f"runtime_event_meta/{self.queue_ids[0]}", event.json()["error"])
        finally:
            with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
                database.execute(
                    "UPDATE runtime_event_meta SET record=? WHERE id=?",
                    (original_metadata, self.queue_ids[0]),
                )
                for _name, sql in triggers:
                    database.execute(sql)
                for _name, sql in indexes:
                    database.execute(sql)
                database.commit()

        broken_body = {
            "agent": self.agent_id, "action": "cancel", "request_id": "broken-receipt",
            "message_id": self.queue_ids[0],
        }
        signature = hashlib.sha256(json.dumps(
            {"agent": self.agent_id, "body": broken_body}, sort_keys=True,
        ).encode()).hexdigest()
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?, ?, '{')",
                (f"queue:{self.agent_id}:broken-receipt", signature),
            )
            database.commit()
        before = no_write_snapshot(self.backend.state)
        receipt = self.request(broken_body)
        self.assertEqual(receipt.status, 500, receipt.body[:1000])
        self.assertIn(f"runtime_operation_receipts/queue:{self.agent_id}:broken-receipt", receipt.json()["error"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("PRAGMA ignore_check_constraints=ON")
            original_agent = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0]
            indexes = database.execute(
                "SELECT name,sql FROM sqlite_master WHERE type='index' AND tbl_name='runtime_agents' AND sql IS NOT NULL"
            ).fetchall()
            for name, _sql in indexes:
                database.execute(f'DROP INDEX "{name}"')
            database.execute(
                "UPDATE runtime_agents SET record='{' WHERE id=?", (self.agent_id,)
            )
            database.commit()
        before = no_write_snapshot(self.backend.state)
        agent = self.request({
            "agent": self.agent_id, "action": "cancel", "message_id": self.queue_ids[0],
        })
        self.assertEqual(agent.status, 500, agent.body[:1000])
        self.assertIn(f"runtime_agents/{self.agent_id}", agent.json()["error"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?", (original_agent, self.agent_id)
            )
            for _name, sql in indexes:
                database.execute(sql)
            database.commit()

    def test_reorder_ties_remain_before_the_system_event_boundary(self):
        from codex_sync_entities import register_functions

        system_id = f"system-boundary-{uuid.uuid4().hex[:8]}"
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            register_functions(database)
            record = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0])
            epoch = record["epoch"]
            database.execute(
                "UPDATE runtime_events SET created=10 WHERE id IN (?,?)",
                (self.queue_ids[0], self.queue_ids[1]),
            )
            database.execute(
                "UPDATE runtime_events SET created=11 WHERE id=?", (self.queue_ids[2],)
            )
            database.execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) "
                "VALUES (?,?,'work_update','System', 'pending',10,?,NULL,NULL)",
                (system_id, self.agent_id, epoch),
            )
            database.commit()
        current = self.get_queue().json()
        ordered = [self.queue_ids[1], self.queue_ids[0], self.queue_ids[2]]
        response = self.request({
            "agent": self.agent_id, "action": "reorder", "request_id": "tie-boundary-order",
            "expected_revision": current["revision"], "ordered_ids": ordered,
        })
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual([item["id"] for item in self.get_queue().json()["items"]], ordered)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            q1 = database.execute("SELECT created FROM runtime_events WHERE id=?", (self.queue_ids[1],)).fetchone()[0]
            q0 = database.execute("SELECT created FROM runtime_events WHERE id=?", (self.queue_ids[0],)).fetchone()[0]
            system = database.execute("SELECT created FROM runtime_events WHERE id=?", (system_id,)).fetchone()[0]
            self.assertLess(q1, q0)
            self.assertLess(q0, system)
            self.assertEqual(system, 10)

    def test_z_first_and_send_now_move_the_requested_message(self):
        current = self.get_queue().json()
        before_ids = [item["id"] for item in current["items"]]
        first_id = before_ids[-1]
        first = self.request({
            "agent": self.agent_id, "action": "first", "request_id": "first-1",
            "expected_revision": current["revision"], "message_id": first_id,
        })
        self.assertEqual(first.status, 200, first.body[:1000])
        after_first = self.get_queue().json()
        self.assertEqual([item["id"] for item in after_first["items"]], [first_id, *before_ids[:-1]])

        current = after_first
        send_now_id = current["items"][-1]["id"]
        send_now_body = {
            "agent": self.agent_id, "action": "send_now", "request_id": "send-now-1",
            "expected_revision": current["revision"], "message_id": send_now_id,
        }
        send_now = self.request(send_now_body)
        self.assertEqual(send_now.status, 200, send_now.body[:1000])
        replay = self.request(send_now_body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["status"], send_now.json()["status"])
        self.assertEqual(replay.json()["revision"], send_now.json()["revision"])
        self.assertEqual(replay.json()["capabilities"], send_now.json()["capabilities"])
        queued = self.get_queue().json()["items"]
        self.assertEqual(queued[0]["id"], send_now_id)
        self.assertEqual(queued[0]["delivery"], "steer")


if __name__ == "__main__":
    unittest.main()
