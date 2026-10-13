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


def coordination_snapshot(path: Path):
    with sqlite3.connect(path) as database:
        result = {}
        for table in (
            "groups", "graph_edges", "messages", "sync_entities", "sync_documents",
            "sync_versions", "sync_entity_meta", "runtime_rooms", "runtime_chat_messages",
            "runtime_events", "runtime_event_meta", "runtime_items",
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


class MessagesHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_agent(cls.backend)
        cls.room_id = "31313131-3131-3131-3131-313131313131"
        cls.seed_persisted_chat()
        cls.backend.start()

    @classmethod
    def seed_persisted_chat(cls):
        class QuietRuntime(_fixture.Runtime):
            def schedule(self):
                pass

        runtime = QuietRuntime(cls.backend.state, _fixture.FakeServer)
        try:
            room = {"id": cls.room_id, "kind": "broadcast", "rootId": cls.agent_id,
                    "updated": 0.5, "userHidden": False}
            with runtime.db() as database:
                runtime.put(database, "rooms", room)
                database.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
                                 ("41414141-4141-4141-4141-414141414141", cls.room_id,
                                  cls.agent_id, "first", 1.0, "{}"))
                database.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
                                 ("42424242-4242-4242-4242-424242424242", cls.room_id,
                                  cls.agent_id, "second", 2.0, json.dumps({cls.agent_id: "queued"})))
                database.execute("INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)",
                                 ("chat:42424242-4242-4242-4242-424242424242:" + cls.agent_id,
                                  cls.agent_id, "agent_message", "{}", "delivered", 2.0, 0, None, None))
                database.execute("INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)",
                                 ("receipt-1", cls.agent_id, "user", "hello", "delivered", 3.0, 0, None, None))
                database.execute("INSERT INTO runtime_event_meta VALUES(?,?)", ("receipt-1", "{}"))
                database.execute("INSERT INTO runtime_items VALUES(?,?,?,?)",
                                 (cls.agent_id + ":receipt-1", cls.agent_id,
                                  json.dumps({"id": cls.agent_id + ":receipt-1", "role": "user"}), 3.0))
        finally:
            runtime.close()

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

        replay = self.backend.request("POST", "/api/messages", body=body, headers=self.headers())
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
        self.assertIn("event-driver/message-send", direct.json()["error"])
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

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_agent_send_refuses_without_event_driver_and_writes_nothing(self):
        before = coordination_snapshot(self.backend.state / "canvas.sqlite3")
        response = self.backend.request("POST", "/api/messages", body={
            "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "room": self.agent_id,
            "text": "Needs a mailbox consumer",
        }, headers=self.headers())
        self.assertEqual(response.status, 501, response.body[:1000])
        self.assertIn("event-driver/message-send", response.json()["error"])
        self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), before)

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

    def test_persisted_agent_chat_pagination_names_overlay_and_visibility(self):
        response = self.backend.request("GET", f"/api/agent-chat?room={self.room_id}&limit=1",
                                        headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["messages"][0]["text"], "second")
        message = response.json()["messages"][0]
        self.assertEqual(message["senderName"], "Coordination contract agent")
        self.assertEqual(message["deliveries"][self.agent_id], "delivered")
        self.assertIsNotNone(response.json()["nextBefore"])
        feed = self.backend.request(
            "GET", f"/api/agent-chat?room=feed:{self.agent_id}&limit=1",
            headers=self.headers(),
        )
        self.assertEqual(feed.status, 200, feed.body[:1000])
        self.assertEqual(feed.json()["messages"][0]["text"], "second")
        older = self.backend.request("GET", f"/api/agent-chat?room={self.room_id}&before={message['seq']}&limit=1",
                                     headers=self.headers())
        self.assertEqual(older.status, 200, older.body[:1000])
        self.assertEqual(older.json()["messages"][0]["text"], "first")
        refused = self.backend.request("GET", "/api/agent-chat?room=not-a-room", headers=self.headers())
        self.assertEqual(refused.status, 400, refused.body[:1000])

    def test_receipts_read_python_written_event_and_room_delete_projects_atomically(self):
        receipt = self.backend.request("GET", "/api/messages/receipts?agent=" + self.agent_id + "&ids=[%22receipt-1%22]",
                                       headers=self.headers())
        self.assertEqual(receipt.status, 200, receipt.body[:1000])
        self.assertEqual(receipt.json()["items"], [{"id": "receipt-1", "status": "delivered",
                                                   "error": None, "materialized": True}])
        path = self.backend.state / "canvas.sqlite3"
        before = coordination_snapshot(path)
        deleted = self.backend.request("POST", "/api/room/delete", body={"id": self.room_id}, headers=self.headers())
        self.assertEqual(deleted.status, 200, deleted.body[:1000])
        after = coordination_snapshot(path)
        self.assertNotEqual(before["runtime_rooms"], after["runtime_rooms"])
        self.assertNotEqual(before["sync_entities"], after["sync_entities"])
        with sqlite3.connect(path) as database:
            projected = json.loads(database.execute(
                "SELECT payload FROM sync_entities WHERE collection='room' AND id=?",
                (self.room_id,),
            ).fetchone()[0])["value"]
        self.assertTrue(projected["userHidden"])
        self.assertEqual(projected["id"], self.room_id)
        for table in ("runtime_chat_messages", "runtime_events", "runtime_event_meta", "runtime_items"):
            self.assertEqual(before[table], after[table], table)
        replay_conflict = self.backend.request("POST", "/api/room/delete", body={"id": "missing"}, headers=self.headers())
        self.assertEqual(replay_conflict.status, 400, replay_conflict.body[:1000])
        self.assertEqual(after, coordination_snapshot(path))

    def test_deleted_actor_cannot_read_receipts_and_refusal_writes_nothing(self):
        self.backend.stop()
        runtime = _fixture.Runtime(self.backend.state, _fixture.FakeServer)
        try:
            actor = runtime.create({
                "name": "Deleted receipts actor", "cwd": str(self.backend.cwd),
                "prompt": "Seed deleted actor receipt refusal.", "autoWake": False,
            }, defer=True)
            actor["deletedAt"] = 123.0
            with runtime.db() as database:
                runtime.put(database, "agents", actor)
            deleted_id = actor["id"]
        finally:
            runtime.close()
        self.backend.start()
        before = coordination_snapshot(self.backend.state / "canvas.sqlite3")
        response = self.backend.request(
            "GET", f"/api/messages/receipts?agent={deleted_id}&ids=%5B%22receipt-1%22%5D",
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(coordination_snapshot(self.backend.state / "canvas.sqlite3"), before)


if __name__ == "__main__":
    unittest.main()
