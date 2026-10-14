#!/usr/bin/env python3
"""Read Python-written transcript rows through both HTTP backends."""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import unittest
from contextlib import closing
from pathlib import Path
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_session_names import identity as native_name_identity


FIXTURE = "restart-mid-turn-supervisor-replay.json"
SEARCH_CORPUS = Path(__file__).parent / "fixtures" / "transcript-search-corpus.json"


def _read_rows(path, agent: str) -> list[tuple[str, str]]:
    uri = f"file:{path}?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as database:
        return database.execute(
            "SELECT id,record FROM runtime_items WHERE agent=? ORDER BY created,id",
            (agent,),
        ).fetchall()


def _snapshot(path) -> tuple[int, int, tuple[tuple[object, ...], ...]]:
    uri = f"file:{path}?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as database:
        items = database.execute("SELECT count(*) FROM runtime_items").fetchone()[0]
        events = database.execute("SELECT count(*) FROM runtime_events").fetchone()[0]
        entities = database.execute(
            "SELECT collection,id,seq,hash,payload,deleted FROM sync_entities ORDER BY collection,id"
        ).fetchall()
        return items, events, tuple(entities)


def _write_item(path, agent: str, identity: str, record: dict, created: float) -> None:
    with closing(sqlite3.connect(path)) as database:
        database.execute(
            "INSERT INTO runtime_items (id,agent,record,created) VALUES (?,?,?,?)",
            (identity, agent, json.dumps(record), created),
        )
        database.commit()


def _write_legacy_search(database, agent: str, record: dict, body: str) -> None:
    """Seed a pre-migration search body using the active runtime schema."""
    identity = record["id"]
    columns = {row[1] for row in database.execute("PRAGMA table_info(runtime_search)")}
    values = {
        "id": identity,
        "agent": agent,
        "kind": record.get("title") or record.get("role") or "message",
        "body": body,
    }
    names = [name for name in values if name in columns]
    if not {"id", "body"}.issubset(names):
        raise AssertionError("runtime_search is missing its id/body columns")
    fields = ",".join(names)
    placeholders = ",".join("?" for _ in names)
    cursor = database.execute(
        f"INSERT INTO runtime_search ({fields}) VALUES ({placeholders})",
        [values[name] for name in names],
    )
    if database.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search_rows'"
    ).fetchone():
        database.execute(
            "INSERT INTO runtime_search_rows (id,search_rowid) VALUES (?,?) "
            "ON CONFLICT(id) DO UPDATE SET search_rowid=excluded.search_rowid",
            (identity, cursor.lastrowid),
        )


class TranscriptHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        rust_command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
        if rust_command:
            # Populate the disposable root with the actual Python runtime before
            # opening that same state with Rust.
            os.environ.pop("STUDIO_HTTP_BACKEND_COMMAND", None)
        try:
            cls.backend = start_contract_backend(cls, FIXTURE)
            cls.headers = {
                "Origin": cls.backend.origin or "",
                "X-Canvas-Token": cls.backend.session["token"],
                "X-Studio-API-Schema": cls.backend.schema_hash,
            }
            lead = cls.backend.request(
                "POST",
                "/api/leads",
                body={"cwd": str(cls.backend.cwd), "workspaceMode": "worktree"},
                headers=cls.headers,
            )
            if lead.status != 200:
                raise AssertionError(f"Python transcript writer setup failed: {lead.body[:1000]}")
            cls.agent = lead.json()["id"]
            message = cls.backend.request(
                "POST",
                "/api/messages",
                body={
                    "id": "f13f1a2b-6c8d-4e0f-9123-abcdef012345",
                    "room": cls.agent,
                    "text": "Reply with the fixture result.",
                    "delivery": "queue",
                    "assets": [],
                },
                headers=cls.headers,
            )
            if message.status != 200:
                raise AssertionError(f"Python transcript writer request failed: {message.body[:1000]}")
            cls.backend.wait_until(
                lambda: any(
                    "Recovered output." in record
                    for _, record in _read_rows(cls.backend.state / "canvas.sqlite3", cls.agent)
                ),
                "Python runtime writes the fixture output to transcript state",
            )
            def replay_settled():
                with closing(sqlite3.connect(cls.backend.state / "canvas.sqlite3")) as database:
                    agent = json.loads(database.execute(
                        "SELECT record FROM runtime_agents WHERE id=?", (cls.agent,),
                    ).fetchone()[0])
                    metadata = json.loads(database.execute(
                        "SELECT record FROM runtime_event_meta WHERE id=?",
                        ("f13f1a2b-6c8d-4e0f-9123-abcdef012345",),
                    ).fetchone()[0])
                # Text can arrive before turn completion, the start receipt, or
                # native-name synchronization has made its final durable write.
                return (
                    agent.get("status") == "completed" and not agent.get("inFlight")
                    and metadata.get("timing", {}).get("acceptedAt") is not None
                    and agent.get("nativeNameSynced") == native_name_identity(agent)
                )
            cls.backend.wait_until(replay_settled, "transcript replay and native name settled")
            cls.rows = _read_rows(cls.backend.state / "canvas.sqlite3", cls.agent)
            cls.item_id = next(
                identity for identity, record in cls.rows if "Recovered output." in record
            )
            original = json.loads(next(record for identity, record in cls.rows if identity == cls.item_id))
            original.pop("inputs", None)
            original["truncated"] = False
            cls.case_items = {
                "fulltext": {**original, "id": "transcript-fulltext-case", "text": "preview only", "truncated": True},
                "legacy": {**original, "id": "transcript-legacy-case", "text": "current phrase", "truncated": False},
                "legacy-truncated": {**original, "id": "transcript-legacy-truncated-case", "text": "legacy preview", "truncated": True},
                "unicode": {**original, "id": "transcript-unicode-case", "text": "İ" * 200 + "x", "truncated": False},
                "casefold": {**original, "id": "transcript-casefold-case", "text": "Straße phrase", "truncated": False},
                "metadata": {
                    **original,
                    "id": "transcript-metadata-case",
                    "text": "container text",
                    "turnId": "parent-turn",
                    "truncated": False,
                    "inputs": [{"id": "receipt", "kind": "user", "text": "metadata phrase", "turnId": "input-turn", "truncated": False}],
                },
                "null-restore": {**original, "id": "transcript-null-restore-case", "text": "visible phrase", "truncated": False, "afterRestore": None},
            }
            for index, record in enumerate(cls.case_items.values()):
                _write_item(cls.backend.state / "canvas.sqlite3", cls.agent, record["id"], record, 2_000_000_000 + index)
            with closing(sqlite3.connect(cls.backend.state / "canvas.sqlite3")) as database:
                database.execute(
                    "INSERT INTO runtime_item_fulltext (id,body) VALUES (?,?)",
                    (cls.case_items["fulltext"]["id"], "resolved archive phrase"),
                )
                if database.execute(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search'"
                ).fetchone() is None:
                    database.execute("CREATE TABLE runtime_search (id TEXT, body TEXT)")
                _write_legacy_search(
                    database, cls.agent, cls.case_items["legacy"], "obsolete phrase"
                )
                _write_legacy_search(
                    database, cls.agent, cls.case_items["legacy-truncated"], "legacy archive phrase"
                )
                database.commit()
            cls.bad_records = {
                "text": {**original, "id": "transcript-invalid-text", "text": 42},
                "role": {**original, "id": "transcript-invalid-role", "role": 42, "text": "invalid role phrase"},
                "input": {**original, "id": "transcript-invalid-input", "inputs": [{"kind": "user", "text": 42}]},
                "event": {**original, "id": "transcript-invalid-event", "inputs": [{"id": "bad-text-event", "kind": "user", "text": "preview", "truncated": False}]},
            }
            cls.python_item = cls.backend.request(
                "GET",
                "/api/transcript/item?" + urlencode({"id": cls.agent, "message_id": cls.item_id}),
                headers=cls.headers,
            )
            if cls.python_item.status != 200:
                raise AssertionError(f"Python transcript item read failed: {cls.python_item.body[:1000]}")
            cls.python_input_item = cls.backend.request(
                "GET",
                "/api/transcript/item?" + urlencode({"id": cls.agent, "message_id": f"{cls.agent}:receipt"}),
                headers=cls.headers,
            )
            if cls.python_input_item.status != 200:
                raise AssertionError(f"Python transcript input read failed: {cls.python_input_item.body[:1000]}")
            with closing(sqlite3.connect(cls.backend.state / "canvas.sqlite3")) as database:
                cls.durable_event = database.execute(
                    "SELECT id,kind,text FROM runtime_events WHERE agent=? AND text IS NOT NULL AND text!='' ORDER BY created DESC LIMIT 1",
                    (cls.agent,),
                ).fetchone()
            if cls.durable_event is None:
                raise AssertionError("Python writer did not persist a transcript input event")
            cls.python_search = {}
            for case in json.loads(SEARCH_CORPUS.read_text(encoding="utf-8")):
                query = case["q"] * case.get("repeat", 1)
                path = "/api/transcript/search?" + urlencode({"id": cls.agent, "q": query})
                response = cls.backend.request("GET", path, headers=cls.headers)
                cls.python_search[case["name"]] = (response.status, response.json())
            cls.python_workspace_search = cls.backend.request(
                "GET", "/api/search?" + urlencode({"q": "Recovered output."}),
                headers=cls.headers,
            )
            cls.python_search_item = cls.backend.request(
                "GET", "/api/search/item?" + urlencode({"id": cls.item_id}),
                headers=cls.headers,
            )
            cls.unavailable_chat_ids = ("chat-from-deleted-member", "chat-from-pending-room")
            with closing(sqlite3.connect(cls.backend.state / "canvas.sqlite3")) as database:
                database.execute(
                    "INSERT INTO runtime_rooms (id,record) VALUES (?,?)",
                    ("deleted-member-room", json.dumps({"id": "deleted-member-room", "kind": "private", "members": ["deleted-chat-member"]})),
                )
                database.execute(
                    "INSERT INTO runtime_rooms (id,record) VALUES (?,?)",
                    ("pending-federated-room", json.dumps({"id": "pending-federated-room", "kind": "federated", "members": [cls.agent]})),
                )
                database.execute(
                    "INSERT INTO runtime_federation_rooms (id,record) VALUES (?,?)",
                    ("pending-federated-room", json.dumps({"id": "pending-federated-room", "status": "pending", "localApproved": False, "remoteApproved": False, "localMembers": [cls.agent], "kind": "federated", "peerId": "fixture-peer", "peerLabel": "Fixture peer", "name": "Pending room", "remoteMembers": [], "shareNames": False, "shareStatus": False, "created": 123.0, "updated": 123.0})),
                )
                for identity, room in zip(cls.unavailable_chat_ids, ("deleted-member-room", "pending-federated-room"), strict=True):
                    database.execute(
                        "INSERT INTO runtime_chat_messages (id,room,sender,text,created,deliveries) VALUES (?,?,?,?,?,?)",
                        (identity, room, cls.agent, "private unavailable text", 2_000_000_100.0, "{}"),
                    )
                database.commit()
            cls.python_unavailable_chat_items = {
                identity: cls.backend.request(
                    "GET", "/api/search/item?" + urlencode({"id": identity}),
                    headers=cls.headers,
                )
                for identity in cls.unavailable_chat_ids
            }
            cls.python_transcript_pull = cls.backend.request(
                "GET", f"/api/sync/pull?scope=transcript:{cls.agent}&after=0",
                headers=cls.headers,
            )
            if cls.python_transcript_pull.status != 200:
                raise AssertionError(
                    f"Python transcript sync pull failed: {cls.python_transcript_pull.body[:1000]}"
                )
            if rust_command:
                cls.backend.stop()
                os.environ["STUDIO_HTTP_BACKEND_COMMAND"] = rust_command
                cls.backend.ready_samples.append(cls.backend.start())
                session = cls.backend.request("GET", "/api/session")
                if session.status != 200:
                    raise AssertionError(f"Rust backend did not serve the Python state: {session.body[:1000]}")
                cls.backend.session = session.json()
                cls.headers["Origin"] = cls.backend.origin or ""
                cls.headers["X-Canvas-Token"] = cls.backend.session["token"]
        finally:
            if rust_command:
                os.environ["STUDIO_HTTP_BACKEND_COMMAND"] = rust_command

    def test_reads_python_written_transcript_state(self) -> None:
        path = "/api/transcript/item?" + urlencode(
            {"id": self.agent, "message_id": self.item_id}
        )
        item = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(item.status, 200, item.body[:1000])
        self.assertEqual(item.json()["id"], self.item_id)
        self.assertIn("Recovered output.", item.json()["text"])

        path = "/api/transcript/search?" + urlencode(
            {"id": self.agent, "q": "Recovered output."}
        )
        results = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(results.status, 200, results.body[:1000])
        self.assertTrue(any(row["sourceId"] == self.item_id for row in results.json()["results"]))

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_00_python_rust_differential_for_python_written_transcript(self) -> None:
        item_path = "/api/transcript/item?" + urlencode(
            {"id": self.agent, "message_id": self.item_id}
        )
        rust_item = self.backend.request("GET", item_path, headers=self.headers)
        self.assertEqual(rust_item.status, self.python_item.status)
        self.assertEqual(rust_item.json(), self.python_item.json())

        input_path = "/api/transcript/item?" + urlencode(
            {"id": self.agent, "message_id": f"{self.agent}:receipt"}
        )
        rust_input = self.backend.request("GET", input_path, headers=self.headers)
        self.assertEqual(rust_input.status, self.python_input_item.status, rust_input.body[:1000])
        self.assertEqual(rust_input.json(), self.python_input_item.json())

        for case in json.loads(SEARCH_CORPUS.read_text(encoding="utf-8")):
            with self.subTest(case=case["name"]):
                query = case["q"] * case.get("repeat", 1)
                path = "/api/transcript/search?" + urlencode({"id": self.agent, "q": query})
                rust = self.backend.request("GET", path, headers=self.headers)
                python_status, python_body = self.python_search[case["name"]]
                self.assertEqual(rust.status, python_status, rust.body[:1000])
                self.assertEqual(rust.json(), python_body)

        search_path = "/api/search?" + urlencode({"q": "Recovered output."})
        rust_search = self.backend.request("GET", search_path, headers=self.headers)
        self.assertEqual(rust_search.status, self.python_workspace_search.status)
        self.assertEqual(rust_search.json(), self.python_workspace_search.json())

        item_path = "/api/search/item?" + urlencode({"id": self.item_id})
        rust_search_item = self.backend.request("GET", item_path, headers=self.headers)
        self.assertEqual(rust_search_item.status, self.python_search_item.status)
        self.assertEqual(rust_search_item.json(), self.python_search_item.json())

        pull = self.backend.request(
            "GET", f"/api/sync/pull?scope=transcript:{self.agent}&after=0",
            headers=self.headers,
        )
        self.assertEqual(pull.status, 200, pull.body[:1000])
        expected_documents = self.python_transcript_pull.json()["documents"]
        actual_documents = pull.json()["documents"]
        self.assertEqual(len(actual_documents), len(expected_documents))
        for actual, expected in zip(actual_documents, expected_documents, strict=True):
            self.assertEqual(
                {
                    "id": actual["id"], "payload": json.loads(actual["payload"]),
                    "_deleted": actual["_deleted"],
                },
                {
                    "id": expected["id"], "payload": json.loads(expected["payload"]),
                    "_deleted": expected["_deleted"],
                },
            )
        replay_snapshot = _snapshot(self.backend.state / "canvas.sqlite3")
        replay = self.backend.request(
            "GET", f"/api/sync/pull?scope=transcript:{self.agent}&after=0",
            headers=self.headers,
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(_snapshot(self.backend.state / "canvas.sqlite3"), replay_snapshot)
        self.assertEqual(replay.json()["documents"], pull.json()["documents"])

    def test_search_item_refuses_unavailable_chat_rooms(self) -> None:
        for identity in self.unavailable_chat_ids:
            with self.subTest(identity=identity):
                response = self.backend.request(
                    "GET", "/api/search/item?" + urlencode({"id": identity}),
                    headers=self.headers,
                )
                python = self.python_unavailable_chat_items[identity]
                self.assertEqual(response.status, python.status, response.body[:1000])
                self.assertEqual(response.status, 400)
                self.assertEqual(response.json(), python.json())

    def test_unknown_transcript_item_does_not_write_state(self) -> None:
        before = no_write_snapshot(self.backend.state)
        path = "/api/transcript/item?" + urlencode(
            {"id": self.agent, "message_id": "missing-transcript-item"}
        )
        response = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_active_transcript_pull_names_event_driver_without_writes(self) -> None:
        database_path = self.backend.state / "canvas.sqlite3"
        with closing(sqlite3.connect(database_path)) as database:
            original = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent,)
            ).fetchone()[0]
            revision_row = database.execute(
                "SELECT revision FROM runtime_transcript_revisions WHERE agent=?",
                (self.agent,),
            ).fetchone()
            database.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.status','running') WHERE id=?",
                (self.agent,),
            )
            database.commit()
        before = _snapshot(database_path)
        response = self.backend.request(
            "GET", f"/api/sync/pull?scope=transcript:{self.agent}&after=0",
            headers=self.headers,
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertIn("documents", response.json())
        self.assertIn("seq", response.json()["checkpoint"])
        with closing(sqlite3.connect(database_path)) as database:
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?", (original, self.agent)
            )
            if revision_row is not None:
                database.execute(
                    "UPDATE runtime_transcript_revisions SET revision=? WHERE agent=?",
                    (revision_row[0], self.agent),
                )
            else:
                database.execute(
                    "DELETE FROM runtime_transcript_revisions WHERE agent=?",
                    (self.agent,),
                )
            database.commit()

    def test_truncated_container_resolves_from_durable_input_event(self) -> None:
        identity = "transcript-event-container-case"
        event_id, kind, event_text = self.durable_event
        record = {
            **self.case_items["fulltext"],
            "id": identity,
            "text": "container preview",
            "truncated": True,
            "inputs": [
                {"id": event_id, "kind": kind, "text": "input preview", "truncated": True}
            ],
        }
        _write_item(self.backend.state / "canvas.sqlite3", self.agent, identity, record, 2_000_000_310)
        path = "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": identity})
        response = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["text"], event_text)
        self.assertFalse(response.json()["truncated"])
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("DELETE FROM runtime_items WHERE id=?", (identity,))
            database.commit()

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_parent_identity_and_truncation_fields_are_validated(self) -> None:
        cases = (
            ("wrong-id-type", {"id": 3, "truncated": False}),
            ("mismatched-id", {"id": "different-sql-id", "truncated": False}),
            ("wrong-truncated-type", {"id": "parent-truncated", "truncated": 1}),
        )
        for offset, (name, fields) in enumerate(cases):
            identity = f"transcript-{name}-case"
            record = {
                **self.case_items["fulltext"],
                **fields,
                "text": "preview",
                "inputs": [
                    {"id": "receipt", "kind": "user", "text": "input", "truncated": False}
                ],
            }
            _write_item(self.backend.state / "canvas.sqlite3", self.agent, identity, record, 2_000_000_320 + offset)
            path = "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": identity})
            response = self.backend.request("GET", path, headers=self.headers)
            with self.subTest(case=name):
                self.assertEqual(response.status, 500, response.body[:1000])
                self.assertIn("runtime_items", response.json().get("error", ""))
            with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
                database.execute("DELETE FROM runtime_items WHERE id=?", (identity,))
                database.commit()

    def test_schema_valid_control_query_is_valid_and_matches_nothing(self) -> None:
        path = "/api/transcript/search?" + urlencode({"id": self.agent, "q": "\x1c\x1d"})
        response = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json(), {"results": [], "truncated": False})

    def test_item_and_search_share_input_hydration(self) -> None:
        identity = f"{self.agent}:receipt"
        item_path = "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": identity})
        item_response = self.backend.request("GET", item_path, headers=self.headers)
        self.assertEqual(item_response.status, 200, item_response.body[:1000])
        query = item_response.json()["text"]
        search_path = "/api/transcript/search?" + urlencode({"id": self.agent, "q": query})
        search_response = self.backend.request("GET", search_path, headers=self.headers)
        self.assertEqual(search_response.status, 200, search_response.body[:1000])
        candidate = next(result for result in search_response.json()["results"] if result["id"] == identity)
        for field in ("id", "text", "role", "turnId"):
            self.assertEqual(candidate[field], item_response.json().get(field), field)

    def test_full_text_resolution_and_legacy_fallback(self) -> None:
        path = "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": self.case_items["fulltext"]["id"]})
        full = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(full.status, 200, full.body[:1000])
        self.assertEqual(full.json()["text"], "resolved archive phrase")
        self.assertFalse(full.json()["truncated"])

        preview = {**self.case_items["fulltext"], "id": "transcript-preview-case", "text": "unavailable preview phrase", "truncated": True}
        _write_item(self.backend.state / "canvas.sqlite3", self.agent, preview["id"], preview, 2_000_000_300)
        path = "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": preview["id"]})
        missing = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(missing.status, 400, missing.body[:1000])
        truncated_input = {
            **self.case_items["fulltext"],
            "id": "transcript-input-preview-case",
            "text": "container preview",
            "truncated": False,
            "inputs": [{"kind": "user", "text": "input preview", "truncated": True}],
        }
        _write_item(self.backend.state / "canvas.sqlite3", self.agent, truncated_input["id"], truncated_input, 2_000_000_301)
        path = "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": f"{truncated_input['id']}:0"})
        missing_input = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(missing_input.status, 400, missing_input.body[:1000])
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("DELETE FROM runtime_items WHERE id=?", (truncated_input["id"],))
            database.commit()
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("DELETE FROM runtime_items WHERE id=?", (preview["id"],))
            database.commit()

        current = self.backend.request(
            "GET", "/api/transcript/search?" + urlencode({"id": self.agent, "q": "current phrase"}), headers=self.headers
        )
        self.assertEqual(current.status, 200, current.body[:1000])
        result = next(row for row in current.json()["results"] if row["sourceId"] == self.case_items["legacy"]["id"])
        self.assertEqual(result["text"], "current phrase")
        obsolete = self.backend.request(
            "GET", "/api/transcript/search?" + urlencode({"id": self.agent, "q": "obsolete phrase"}), headers=self.headers
        )
        self.assertFalse(any(row["sourceId"] == self.case_items["legacy"]["id"] for row in obsolete.json()["results"]))
        legacy = self.backend.request(
            "GET", "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": self.case_items["legacy-truncated"]["id"]}), headers=self.headers
        )
        self.assertEqual(legacy.status, 200, legacy.body[:1000])
        self.assertEqual(legacy.json()["text"], "legacy archive phrase")
        self.assertFalse(legacy.json()["truncated"])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_search_preserves_candidate_metadata_and_clamps_unicode_snippet(self) -> None:
        metadata = self.backend.request(
            "GET", "/api/transcript/search?" + urlencode({"id": self.agent, "q": "metadata phrase"}), headers=self.headers
        )
        self.assertEqual(metadata.status, 200, metadata.body[:1000])
        result = next(row for row in metadata.json()["results"] if row["sourceId"] == self.case_items["metadata"]["id"])
        self.assertEqual(result["clientMessageId"], "receipt")
        self.assertEqual(result["turnId"], "input-turn")

        unicode = self.backend.request(
            "GET", "/api/transcript/search?" + urlencode({"id": self.agent, "q": "x"}), headers=self.headers
        )
        self.assertEqual(unicode.status, 200, unicode.body[:1000])
        unicode_result = next(row for row in unicode.json()["results"] if row["sourceId"] == self.case_items["unicode"]["id"])
        self.assertEqual(unicode_result["excerpt"], "")

        folded = self.backend.request(
            "GET", "/api/transcript/search?" + urlencode({"id": self.agent, "q": "STRASSE"}), headers=self.headers
        )
        self.assertFalse(any(row["sourceId"] == self.case_items["casefold"]["id"] for row in folded.json()["results"]))

        visible = self.backend.request(
            "GET", "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": self.case_items["null-restore"]["id"]}), headers=self.headers
        )
        self.assertEqual(visible.status, 200, visible.body[:1000])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_fields_and_malformed_json_name_the_object(self) -> None:
        path = "/api/transcript/search?" + urlencode({"id": self.agent, "q": "invalid"})
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            for (trigger,) in database.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name='runtime_items'"
            ).fetchall():
                database.execute(f'DROP TRIGGER "{trigger}"')
            database.execute(
                "INSERT INTO runtime_items (id,agent,record,created) VALUES (?,?,?,?)",
                ("transcript-malformed-record", self.agent, "{", 2_000_000_200),
            )
            database.commit()
        malformed = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(malformed.status, 500, malformed.body[:1000])
        self.assertIn("runtime_items", malformed.json().get("error", ""))
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("DELETE FROM runtime_items WHERE id='transcript-malformed-record'")
            for index, record in enumerate(self.bad_records.values()):
                database.execute(
                    "INSERT INTO runtime_items (id,agent,record,created) VALUES (?,?,?,?)",
                    (record["id"], self.agent, json.dumps(record), 2_000_000_201 + index),
                )
            database.commit()
        invalid_search = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(invalid_search.status, 500, invalid_search.body[:1000])
        self.assertIn("runtime_items", invalid_search.json().get("error", ""))
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute(
                "DELETE FROM runtime_items WHERE id IN (?,?,?,?)",
                tuple(record["id"] for record in self.bad_records.values()),
            )
            database.execute(
                "INSERT INTO runtime_items (id,agent,record,created) VALUES (?,?,?,?)",
                ("transcript-invalid-record-column", self.agent, sqlite3.Binary(b"{}"), 2_000_000_203),
            )
            database.commit()
        bad_column = self.backend.request("GET", path, headers=self.headers)
        self.assertEqual(bad_column.status, 500, bad_column.body[:1000])
        self.assertIn("runtime_items", bad_column.json().get("error", ""))
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("DELETE FROM runtime_items WHERE id='transcript-invalid-record-column'")
            database.execute(
                "UPDATE runtime_item_fulltext SET body=? WHERE id=?",
                (sqlite3.Binary(b"bad"), self.case_items["fulltext"]["id"]),
            )
            database.commit()
        bad_fulltext = self.backend.request(
            "GET", "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": self.case_items["fulltext"]["id"]}), headers=self.headers
        )
        self.assertEqual(bad_fulltext.status, 500, bad_fulltext.body[:1000])
        self.assertIn("runtime_item_fulltext", bad_fulltext.json().get("error", ""))
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute(
                "UPDATE runtime_item_fulltext SET body=? WHERE id=?",
                ("resolved archive phrase", self.case_items["fulltext"]["id"]),
            )
            database.execute(
                "UPDATE runtime_search SET body=? WHERE id=?",
                (sqlite3.Binary(b"bad"), self.case_items["legacy-truncated"]["id"]),
            )
            database.commit()
        bad_legacy = self.backend.request(
            "GET", "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": self.case_items["legacy-truncated"]["id"]}), headers=self.headers
        )
        self.assertEqual(bad_legacy.status, 500, bad_legacy.body[:1000])
        self.assertIn("runtime_search", bad_legacy.json().get("error", ""))
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute(
                "UPDATE runtime_search SET body=? WHERE id=?",
                ("legacy archive phrase", self.case_items["legacy-truncated"]["id"]),
            )
            database.commit()
            for (trigger,) in database.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name='runtime_events'"
            ).fetchall():
                database.execute(f'DROP TRIGGER "{trigger}"')
            database.execute(
                "INSERT INTO runtime_events (id,agent,epoch,kind,status,created,text) VALUES (?,?,?,?,?,?,?)",
                ("invalid-input", self.agent, 1, "user", "delivered", 2_000_000_205, sqlite3.Binary(b"x")),
            )
            database.execute(
                "INSERT INTO runtime_events (id,agent,epoch,kind,status,created,text) VALUES (?,?,?,?,?,?,?)",
                ("bad-text-event", self.agent, 1, "user", "delivered", 2_000_000_206, sqlite3.Binary(b"x")),
            )
            record = self.bad_records["event"]
            database.execute(
                "INSERT INTO runtime_items (id,agent,record,created) VALUES (?,?,?,?)",
                (record["id"], self.agent, json.dumps(record), 2_000_000_204),
            )
            database.commit()
        event = self.backend.request(
            "GET", "/api/transcript/item?" + urlencode({"id": self.agent, "message_id": f"{self.agent}:bad-text-event"}), headers=self.headers
        )
        self.assertEqual(event.status, 500, event.body[:1000])
        self.assertIn("runtime_events", event.json().get("error", ""))
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("DELETE FROM runtime_items WHERE id=?", (record["id"],))
            database.execute("DELETE FROM runtime_events WHERE id IN ('invalid-input','bad-text-event')")
            database.commit()

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_unavailable_event_and_workspace_routes_name_their_dependencies(self) -> None:
        expected = {
            "/api/transcript": "transcript/event-driver",
            "/api/transcript/page": "transcript/event-driver",
            "/api/search": "transcript/room-search",
        }
        for path, dependency in expected.items():
            with self.subTest(path=path):
                response = self.backend.request(
                    "GET",
                    path
                    + "?"
                    + urlencode(
                        (
                            {"id": self.agent, "message_id": self.item_id}
                            if path == "/api/search/item"
                            else {"id": self.agent}
                        )
                        if path.startswith("/api/transcript")
                        else {"q": "fixture"}
                    ),
                    headers=self.headers,
                )
                self.assertEqual(response.status, 200, response.body[:1000])
                self.assertNotIn("error", response.json())


if __name__ == "__main__":
    unittest.main(verbosity=2)
