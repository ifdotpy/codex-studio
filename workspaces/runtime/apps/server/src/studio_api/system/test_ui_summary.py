"""Exercise the read-only summary against durable entity records."""
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from typing import Iterator, cast

from .ui_summary import read_summary


class UiSummaryTests(unittest.TestCase):
    def test_durable_unread_notifications_and_no_read_side_effect(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "canvas.sqlite3"
            agent = {"id": "lead", "name": "Lead", "source": "managed", "isLead": True, "cwd": "/project", "threadId": "thread", "lastCompletedTurn": "turn", "lastCompletedTurnStatus": "completed", "status": "completed", "tail": "Reply", "secret": "do-not-return", "readState": {"threadId": "thread", "turnId": "older", "read": True}}
            rows = [("agent", "lead", agent), ("agent", "anchor", {**agent, "id": "anchor", "remoteAnchor": {"home": "other"}}), ("request", "question", {"id": "question", "agent": "lead", "status": "pending", "params": {"questions": [{"question": "Continue?"}]}}), ("complaint", "complaint", {"id": "complaint", "leadId": "lead", "needsUserResponse": True, "title": "Message", "version": 2})]
            with sqlite3.connect(path) as db:
                db.execute("CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)")
                db.executemany("INSERT INTO sync_entities VALUES(?,?,?,0)", [(collection, key, json.dumps({"value": value})) for collection, key, value in rows])
            @contextmanager
            def connect() -> Iterator[sqlite3.Connection]:
                db = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)
                try:
                    yield db
                finally:
                    db.close()
            first = read_summary(connect)
            second = read_summary(connect)
            self.assertEqual(first, second)
            self.assertTrue(first.chats[0].unread)
            self.assertEqual([row.id for row in first.chats], ["lead"])
            self.assertEqual({row.id for row in first.alerts}, {"completed:lead:turn", "request:question", "complaint:complaint:2"})
            self.assertNotIn("secret", first.model_dump_json())
            with sqlite3.connect(path) as db:
                saved = json.loads(db.execute("SELECT payload FROM sync_entities WHERE id='lead'").fetchone()[0])["value"]
                self.assertEqual(saved, agent)
                saved["readState"]["turnId"] = "turn"
                db.execute("UPDATE sync_entities SET payload=? WHERE id='lead'", (json.dumps({"value": saved}),))
            self.assertFalse(read_summary(connect).chats[0].unread)

    def test_missing_projection_does_not_create_tables(self) -> None:
        with sqlite3.connect(":memory:") as db:
            @contextmanager
            def connect() -> Iterator[sqlite3.Connection]:
                yield db
            self.assertFalse(read_summary(connect).ready)
            self.assertEqual(db.execute("SELECT COUNT(*) FROM sqlite_master").fetchone()[0], 0)

    def test_actual_route_returns_only_summary_and_refuses_writes(self) -> None:
        from types import SimpleNamespace
        from fastapi import FastAPI
        from fastapi.testclient import TestClient
        from starlette.responses import JSONResponse
        from .router import create_router
        from studio_api.context import ApiContext
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "canvas.sqlite3"
            with sqlite3.connect(path) as db:
                db.execute("CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)")
                db.execute("INSERT INTO sync_entities VALUES('agent','worker',?,0)", (json.dumps({"value": {"id": "worker", "inFlight": True}}),))
            context = SimpleNamespace(canvas=SimpleNamespace(db=path), send=lambda _request, value: JSONResponse(value))
            app = FastAPI()
            app.include_router(create_router(cast(ApiContext, context)))
            with TestClient(app) as client:
                response = client.get("/api/ui-summary")
                self.assertEqual(response.status_code, 200)
                self.assertTrue(response.json()["busy"])
                self.assertEqual(set(response.json()), {
                    "ready", "busy", "system", "agentsRunning", "projects", "chats", "alerts", "accounts",
                })
                self.assertEqual(response.json()["accounts"], [])
                self.assertEqual(response.json()["agentsRunning"], 1)
                self.assertEqual(client.post("/api/ui-summary", json={}).status_code, 405)

    def test_actual_route_returns_only_public_account_metadata(self) -> None:
        from types import SimpleNamespace
        from fastapi import FastAPI
        from fastapi.testclient import TestClient
        from starlette.responses import JSONResponse
        from .router import create_router
        from studio_api.context import ApiContext
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "canvas.sqlite3"
            with sqlite3.connect(path) as db:
                db.execute("CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)")
            rows = [{"id": "claude-a", "provider": "claude", "email": "a@example.invalid",
                     "plan": "Max", "status": "ready", "label": "Claude A", "disconnected": True,
                     "home": "/private/home", "accountId": "secret"}]
            runtime = SimpleNamespace(accounts=SimpleNamespace(snapshot=lambda *, refresh: {
                "accounts": rows, "defaultAccountKey": "claude-a"}))
            context = SimpleNamespace(canvas=SimpleNamespace(db=path), runtime=runtime,
                                      send=lambda _request, value: JSONResponse(value))
            app = FastAPI()
            app.include_router(create_router(cast(ApiContext, context)))
            with TestClient(app) as client:
                response = client.get("/api/ui-summary")
                self.assertEqual(response.status_code, 200)
                self.assertEqual(response.json()["accounts"], [{
                    "provider": "claude", "email": "a@example.invalid", "plan": "Max",
                    "status": "signedOut", "label": "Claude A", "isDefault": True,
                }])
