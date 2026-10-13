#!/usr/bin/env python3
"""HTTP contracts for durable chats, connections and group messages."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import sqlite3
import tempfile
import unittest
from contextlib import closing
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

HERE = Path(__file__).resolve().parent
SERVER_SOURCE = HERE.parent / "src"
import sys
sys.path.insert(0, str(SERVER_SOURCE))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend

_fixture_spec = importlib.util.spec_from_file_location("messages_runtime_fixture", HERE / "runtime-contract.py")
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


def seed_agent(backend) -> str:
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    try:
        agent = runtime.create({
            "name": "Coordination contract agent",
            "cwd": str(backend.cwd),
            "prompt": "Seed an idle agent for chat HTTP contracts.",
            "autoWake": bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")),
        }, defer=True)
        agent.update(autoWake=bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")), status="idle")
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
        return agent["id"]
    finally:
        runtime.close()


def hold_agent_busy(backend, agent_id: str) -> None:
    backend.stop()
    with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as database:
        record = json.loads(database.execute(
            "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
        ).fetchone()[0])
        record.update(status="running", inFlight=True)
        database.execute(
            "UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(record), agent_id),
        )
    backend.start()


def coordination_snapshot(path: Path):
    with sqlite3.connect(path) as database:
        result = {}
        for table in (
            "groups", "graph_edges", "messages", "sync_entities", "sync_documents",
            "sync_versions", "sync_entity_meta",
        ):
            exists = database.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)
            ).fetchone()
            if not exists:
                continue
            result[table] = sorted(database.execute(f'SELECT * FROM "{table}"').fetchall(), key=repr)
        return result


def without_timestamps(value):
    if isinstance(value, dict):
        return {
            key: without_timestamps(item)
            for key, item in value.items()
            if key not in {"lastMessageAt", "updated", "at"}
        }
    if isinstance(value, list):
        return [without_timestamps(item) for item in value]
    return value


def parsed_coordination_snapshot(path: Path):
    with sqlite3.connect(path) as database:
        groups = [(id, name, json.loads(members)) for id, name, members in database.execute(
            "SELECT id,name,members FROM groups ORDER BY id"
        )]
        edges = sorted(database.execute(
            "SELECT id,source,target,kind FROM graph_edges ORDER BY id"
        ).fetchall())
        messages = [
            (id, room, author, text, json.loads(deliveries))
            for id, room, author, text, _at, deliveries in database.execute(
                "SELECT id,room,author,text,at,deliveries FROM messages ORDER BY id"
            )
        ]
        entities = [
            (collection, id, without_timestamps(json.loads(payload)), deleted)
            for collection, id, payload, deleted in database.execute(
                "SELECT collection,id,payload,deleted FROM sync_entities "
                "WHERE collection IN ('chat','edge') ORDER BY collection,id"
            )
        ]
        documents = [
            (scope, id, without_timestamps(json.loads(payload)), deleted)
            for scope, id, payload, deleted in database.execute(
                "SELECT scope,id,payload,deleted FROM sync_documents "
                "WHERE scope IN ('chat','edge') ORDER BY scope,id"
            )
        ]
        versions = sorted(database.execute(
            "SELECT scope,deleted FROM sync_versions WHERE scope IN ('chat','edge')"
        ).fetchall())
        return {
            "groups": groups,
            "graph_edges": edges,
            "messages": messages,
            "sync_entities": entities,
            "sync_documents": documents,
            "sync_versions": versions,
        }


def managed_message_snapshot(path: Path, message_id: str):
    with closing(sqlite3.connect(path)) as database:
        event = database.execute(
            "SELECT id,agent,kind,text,status,epoch,turn_id,error FROM runtime_events WHERE id=?",
            (message_id,),
        ).fetchone()
        metadata = database.execute(
            "SELECT record FROM runtime_event_meta WHERE id=?", (message_id,),
        ).fetchone()
        message = database.execute(
            "SELECT id,room,author,text,at,deliveries FROM messages WHERE id=?", (message_id,),
        ).fetchone()
        record = json.loads(metadata[0]) if metadata else None
        if record:
            record.pop("acceptedAt", None)
            record.pop("timing", None)
        return {
            "event": event,
            "metadata": record,
            "message": None if message is None else (
                *message[:4], json.loads(message[5]),
            ),
        }


def message_write_snapshot(path: Path, message_id: str):
    with closing(sqlite3.connect(path)) as database:
        result = {}
        for table, query in (
            ("event", "SELECT id,agent,kind,text,epoch FROM runtime_events WHERE id=?"),
            ("metadata", "SELECT * FROM runtime_event_meta WHERE id=?"),
            ("receipt", "SELECT * FROM runtime_operation_receipts WHERE id=?"),
            ("message", "SELECT * FROM messages WHERE id=?"),
        ):
            result[table] = database.execute(query, (message_id,)).fetchone()
        if result["metadata"]:
            metadata = json.loads(result["metadata"][1])
            metadata.pop("timing", None)
            result["metadata"] = (result["metadata"][0], metadata)
        return result


def python_direct_message(backend, agent_id: str, message_id: str, text: str, delivery: str):
    backend.stop()
    try:
        with tempfile.TemporaryDirectory(prefix="messages-python-reference-") as directory:
            state = Path(directory) / "state"
            state.mkdir()
            with sqlite3.connect(backend.state / "canvas.sqlite3") as source, sqlite3.connect(
                state / "canvas.sqlite3"
            ) as destination:
                source.backup(destination)
            class QuietRuntime(_fixture.Runtime):
                def schedule(self):
                    pass
            runtime = QuietRuntime(state, _fixture.FakeServer)
            try:
                result = runtime.send(agent_id, text, message_id, delivery=delivery)
                with runtime.db() as database:
                    database.execute(
                        "INSERT OR IGNORE INTO messages VALUES (?,?,?,?,?,?)",
                        (result["id"], agent_id, "user", text.strip(), 1.0,
                         json.dumps({agent_id: result["status"]})),
                    )
            finally:
                runtime.close()
            return managed_message_snapshot(state / "canvas.sqlite3", message_id)
    finally:
        backend.start()


class MessagesHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_agent(cls.backend)
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

    def create_chat(self, name="Team room"):
        body = {
            "id": "12345678-1234-1234-1234-123456789abc",
            "name": name,
            "members": [self.agent_id],
        }
        response = self.backend.request("POST", "/api/chats", body=body, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        return body

    def test_chat_create_replay_conflict_and_refusal_have_no_extra_rows(self):
        body = self.create_chat()
        first = coordination_snapshot(self.backend.state / "canvas.sqlite3")
        replay = self.backend.request("POST", "/api/chats", body=body, headers=self.headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), first)

        conflict = self.backend.request("POST", "/api/chats", body={**body, "name": "Different"}, headers=self.headers())
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), first)

        refusal = self.backend.request("POST", "/api/chats", body={**body, "id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "members": ["missing-agent"]}, headers=self.headers())
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(refusal.status, 501, refusal.body[:1000])
            self.assertIn("agent-discovery/launcher-status", refusal.json()["error"])
        else:
            self.assertEqual(refusal.status, 400, refusal.body[:1000])
        self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), first)

    def test_request_ids_use_the_python_schema_pattern(self):
        room = "a" * 36
        created = self.backend.request("POST", "/api/chats", body={
            "id": room, "name": "Pattern room", "members": [],
        }, headers=self.headers())
        self.assertEqual(created.status, 200, created.body[:1000])
        message = self.backend.request("POST", "/api/messages", body={
            "id": "b" * 36, "room": room, "text": "Pattern message",
        }, headers=self.headers())
        self.assertEqual(message.status, 200, message.body[:1000])
        before = coordination_snapshot(self.backend.state / "canvas.sqlite3")
        for invalid in ("A" * 36, "a" * 35, "g" * 36):
            response = self.backend.request("POST", "/api/chats", body={
                "id": invalid, "name": "Invalid identity", "members": [],
            }, headers=self.headers())
            self.assertEqual(response.status, 400, response.body[:1000])
            self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), before)

    def test_connections_keep_stable_identity_and_refusal_has_no_write(self):
        chat = self.create_chat()
        body = {"source": self.agent_id, "target": chat["id"], "connected": True}
        response = self.backend.request("POST", "/api/connections", body=body, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        edge_id = hashlib.sha256(json.dumps(("chat", self.agent_id, chat["id"])).encode()).hexdigest()[:24]
        self.assertEqual(response.json(), {"id": edge_id, "connected": True})
        replay = self.backend.request("POST", "/api/connections", body=body, headers=self.headers())
        self.assertEqual(replay.json(), response.json())

        before = coordination_snapshot(self.backend.state / "canvas.sqlite3")
        refusal = self.backend.request("POST", "/api/connections", body={**body, "target": "missing-chat"}, headers=self.headers())
        self.assertEqual(refusal.status, 400, refusal.body[:1000])
        self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), before)

        removed = self.backend.request("POST", "/api/connections", body={**body, "connected": False}, headers=self.headers())
        self.assertEqual(removed.status, 200, removed.body[:1000])
        self.assertFalse(removed.json()["connected"])

    def test_group_message_replay_conflict_and_history(self):
        chat = self.create_chat()
        body = {"id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "room": chat["id"], "text": "Hello team"}
        posted = self.backend.request("POST", "/api/messages", body=body, headers=self.headers())
        self.assertEqual(posted.status, 200, posted.body[:1000])
        self.assertEqual(posted.json()["id"], body["id"])
        self.assertIn(posted.json()["status"], {"queued", "delivered"})

        replay = self.backend.request("POST", "/api/messages", body={**body, "delivery": "queue"}, headers=self.headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], body["id"])
        self.assertEqual(replay.json()["text"], "Hello team")

        history = self.backend.request("GET", f"/api/messages?room={chat['id']}", headers=self.headers())
        self.assertEqual(history.status, 200, history.body[:1000])
        self.assertEqual(len(history.json()), 1)
        self.assertEqual(history.json()[0]["id"], body["id"])

        before = coordination_snapshot(self.backend.state / "canvas.sqlite3")
        conflict = self.backend.request("POST", "/api/messages", body={**body, "text": "Changed"}, headers=self.headers())
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), before)

    def test_parsed_write_set_matches_python_canvas(self):
        from codex_canvas import Canvas

        room = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee"
        message = "ffffffff-ffff-ffff-ffff-ffffffffffff"
        with tempfile.TemporaryDirectory(prefix="rust-messages-python-reference-") as directory:
            reference_root = Path(directory)
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as source, sqlite3.connect(
                reference_root / "canvas.sqlite3"
            ) as destination:
                source.backup(destination)
            reference = Canvas(reference_root)
            try:
                reference.create_chat("Parity room", [], room)
                reference.post(room, "Same request", message, notify=False)
            finally:
                del reference

            create = self.backend.request("POST", "/api/chats", body={
                "id": room, "name": "Parity room", "members": [],
            }, headers=self.headers())
            self.assertEqual(create.status, 200, create.body[:1000])
            post = self.backend.request("POST", "/api/messages", body={
                "id": message, "room": room, "text": "Same request",
            }, headers=self.headers())
            self.assertEqual(post.status, 200, post.body[:1000])
            self.assertEqual(
                parsed_coordination_snapshot(self.backend.state / "canvas.sqlite3"),
                parsed_coordination_snapshot(reference_root / "canvas.sqlite3"),
            )

    def test_historical_room_cannot_be_taken_over_by_another_team(self):
        self.backend.stop()
        path = self.backend.state / "canvas.sqlite3"
        room = "12121212-1212-1212-1212-121212121212"
        with sqlite3.connect(path) as database:
            other = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0])
            other.update({"id": "other-team", "rootId": "other-team", "parentId": None})
            database.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?,?)",
                ("other-team", json.dumps(other)),
            )
            database.execute(
                "INSERT INTO messages(id,room,author,text,at,deliveries) VALUES(?,?,?,?,?,?)",
                ("13131313-1313-1313-1313-131313131313", room, "other-team", "old", 1.0, "{}"),
            )
        self.backend.start()
        before = coordination_snapshot(path)
        response = self.backend.request("POST", "/api/chats", body={
            "id": room, "name": "Takeover", "members": [self.agent_id],
        }, headers=self.headers())
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertIn("another team", response.json()["error"])
        self.assertEqual(coordination_snapshot(path), before)

    def test_registered_child_inherits_parent_team_and_managed_alias_is_hidden(self):
        self.backend.stop()
        path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(path) as database:
            agent = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0])
            agent["threadId"] = "same-managed-thread"
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))
            database.execute("INSERT INTO graph_agents(id,record) VALUES(?,?)", (
                "registered-child", json.dumps({"id": "registered-child", "parentId": self.agent_id}),
            ))
            database.execute("INSERT INTO graph_agents(id,record) VALUES(?,?)", (
                "managed-alias", json.dumps({"id": "managed-alias", "threadId": "same-managed-thread"}),
            ))
        self.backend.start()
        inherited = self.backend.request("POST", "/api/chats", body={
            "id": "14141414-1414-1414-1414-141414141414",
            "name": "Inherited team", "members": [self.agent_id, "registered-child"],
        }, headers=self.headers())
        self.assertEqual(inherited.status, 200, inherited.body[:1000])
        before = coordination_snapshot(path)
        alias = self.backend.request("POST", "/api/chats", body={
            "id": "15151515-1515-1515-1515-151515151515",
            "name": "Hidden alias", "members": ["managed-alias"],
        }, headers=self.headers())
        self.assertEqual(alias.status, 400, alias.body[:1000])
        self.assertEqual(coordination_snapshot(path), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_federation_and_managed_send_routes_refuse_before_canvas_writes(self):
        self.backend.stop()
        path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(path) as database:
            database.execute("CREATE TABLE IF NOT EXISTS runtime_federation_rooms(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            database.execute("INSERT OR REPLACE INTO runtime_federation_rooms(id,record) VALUES(?,?)", (
                "federated:peer:room", json.dumps({"id": "federated:peer:room", "status": "approved"}),
            ))
        self.backend.start()
        before = coordination_snapshot(path)
        federation = self.backend.request("POST", "/api/messages", body={
            "id": "16161616-1616-1616-1616-161616161616",
            "room": "federated:peer:room", "text": "",
        }, headers=self.headers())
        self.assertEqual(federation.status, 501, federation.body[:1000])
        self.assertIn("federation-driver/rooms", federation.json()["error"])
        direct = self.backend.request("POST", "/api/messages", body={
            "id": "17171717-1717-1717-1717-171717171717",
            "room": self.agent_id, "text": "x" * 32_000, "assets": [{"attachment": "x"}],
        }, headers=self.headers())
        self.assertEqual(direct.status, 501, direct.body[:1000])
        self.assertEqual(direct.json().get("error"), "Not implemented: requires payload-store/uploads")
        self.assertEqual(coordination_snapshot(path), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_long_and_empty_python_written_messages_are_read_and_bad_deliveries_name_row(self):
        self.backend.stop()
        room = "18181818-1818-1818-1818-181818181818"
        long_id = "19191919-1919-1919-1919-191919191919"
        bad_id = "20202020-2020-2020-2020-202020202020"
        empty_id = "21212121-2121-2121-2121-212121212121"
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("INSERT INTO groups(id,name,members) VALUES(?,?,?)", (room, "History", "[]"))
            database.execute("INSERT INTO messages VALUES(?,?,?,?,?,?)", (
                long_id, room, "user", "x" * 32_000, 1.0, json.dumps({"agent": "sent"}),
            ))
            database.execute("INSERT INTO messages VALUES(?,?,?,?,?,?)", (
                bad_id, room, "user", "", 2.0, "not-json",
            ))
            database.execute("INSERT INTO messages VALUES(?,?,?,?,?,?)", (
                empty_id, room, "user", "", 3.0, json.dumps({"agent": "delivered"}),
            ))
        self.backend.start()
        history = self.backend.request("GET", f"/api/messages?room={room}", headers=self.headers())
        self.assertEqual(history.status, 500, history.body[:1000])
        self.assertIn(f"messages/{bad_id}", history.json()["error"])
        self.assertNotIn(f"messages/{room}", history.json()["error"])
        takeover = self.backend.request("POST", "/api/chats", body={
            "id": room, "name": "History", "members": [],
        }, headers=self.headers())
        self.assertEqual(takeover.status, 500, takeover.body[:1000])
        self.assertIn(f"messages/{bad_id}", takeover.json()["error"])
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("DELETE FROM messages WHERE id=?", (bad_id,))
        history = self.backend.request("GET", f"/api/messages?room={room}", headers=self.headers())
        self.assertEqual(history.status, 200, history.body[:1000])
        rows = {row["id"]: row for row in history.json()}
        self.assertEqual(len(rows[long_id]["text"]), 32_000)
        self.assertEqual(rows[long_id]["deliveries"], {"agent": "sent"})
        self.assertEqual(rows[empty_id]["text"], "")

    def test_managed_send_matches_python_rows_queue_replay_conflict_and_attachment_refusal(self):
        hold_agent_busy(self.backend, self.agent_id)
        message_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
        body = {
            "id": message_id,
            "room": self.agent_id,
            "text": "Needs a pending event",
            "delivery": "after_turn",
        }
        expected = python_direct_message(
            self.backend, self.agent_id, message_id, body["text"], body["delivery"],
        )
        response = self.backend.request("POST", "/api/messages", body=body, headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["id"], message_id)
        self.assertEqual(response.json()["status"], "queued")
        self.assertEqual(managed_message_snapshot(self.backend.state / "canvas.sqlite3", message_id), expected)

        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            queue = self.backend.request("GET", f"/api/queue?agent={self.agent_id}", headers=self.headers())
            self.assertEqual(queue.status, 200, queue.body[:1000])
            self.assertIn(message_id, json.dumps(queue.json()))
            with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
                self.assertIsNotNone(database.execute(
                    "SELECT 1 FROM sync_entities WHERE collection='event' AND id=?", (message_id,),
                ).fetchone())

        before = message_write_snapshot(self.backend.state / "canvas.sqlite3", message_id)
        replay = self.backend.request("POST", "/api/messages", body=body, headers=self.headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], message_id)
        self.assertEqual(message_write_snapshot(self.backend.state / "canvas.sqlite3", message_id), before)
        conflict = self.backend.request("POST", "/api/messages", body={**body, "text": "Changed"}, headers=self.headers())
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(message_write_snapshot(self.backend.state / "canvas.sqlite3", message_id), before)

        for invalid in (
            {**body, "id": "abababab-abab-abab-abab-abababababab", "text": ""},
            {**body, "id": "acacacac-acac-acac-acac-acacacacacac", "text": "x" * 32_001},
        ):
            refused_id = invalid["id"]
            unchanged = message_write_snapshot(self.backend.state / "canvas.sqlite3", refused_id)
            refused = self.backend.request("POST", "/api/messages", body=invalid, headers=self.headers())
            self.assertEqual(refused.status, 400, refused.body[:1000])
            self.assertEqual(message_write_snapshot(self.backend.state / "canvas.sqlite3", refused_id), unchanged)

        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            attachment_id = "adadadad-adad-adad-adad-adadadadadad"
            unchanged = message_write_snapshot(self.backend.state / "canvas.sqlite3", attachment_id)
            attachment = self.backend.request("POST", "/api/messages", body={
                **body, "id": attachment_id, "assets": [{"name": "pending"}],
            }, headers=self.headers())
            self.assertEqual(attachment.status, 501, attachment.body[:1000])
            self.assertEqual(attachment.json().get("error"), "Not implemented: requires payload-store/uploads")
            self.assertEqual(message_write_snapshot(self.backend.state / "canvas.sqlite3", attachment_id), unchanged)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_reads_python_written_chat_and_message_without_loss(self):
        from codex_canvas import Canvas

        self.backend.stop()
        canvas = Canvas(self.backend.state)
        room_id = "cccccccc-cccc-cccc-cccc-cccccccccccc"
        message_id = "dddddddd-dddd-dddd-dddd-dddddddddddd"
        try:
            created = canvas.create_chat("Python seeded room", [self.agent_id], room_id)
            self.assertEqual(created["id"], room_id)
            saved = canvas.post(room_id, "Written by Python", message_id, notify=False)
            self.assertEqual(saved["status"], "delivered")
        finally:
            del canvas

        self.backend.start()
        headers = self.headers()
        history = self.backend.request("GET", f"/api/messages?room={room_id}", headers=headers)
        self.assertEqual(history.status, 200, history.body[:1000])
        self.assertEqual(history.json(), [{
            "id": message_id,
            "room": room_id,
            "author": "user",
            "text": "Written by Python",
            "at": history.json()[0]["at"],
            "deliveries": {},
        }])
        replay = self.backend.request("POST", "/api/chats", body={
            "id": room_id,
            "name": "Python seeded room",
            "members": [self.agent_id],
        }, headers=headers)
        self.assertEqual(replay.status, 200, replay.body[:1000])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_deferred_message_operations_name_their_dependencies(self):
        for method, path, body, dependency in (
            ("GET", "/api/agent-chat?room=missing", None, "coordination-policy/chat-history"),
            ("GET", "/api/messages/receipts", None, "event-driver/message-receipts"),
            ("POST", "/api/room/delete", {"id": "missing"}, "coordination-policy/room-visibility"),
        ):
            response = self.backend.request(method, path, body=body, headers=self.headers())
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn(dependency, response.json()["error"])


if __name__ == "__main__":
    unittest.main()
