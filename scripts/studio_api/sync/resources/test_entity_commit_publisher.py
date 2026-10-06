"""Every supported instrumented commit publishes its entity sequence once."""

from __future__ import annotations

import asyncio
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from uuid import uuid4

from codex_budget import _save as save_budget
from codex_canvas import Canvas
from codex_rules import RulesMixin
from codex_sqlite import connect
from codex_sync_entities import ensure_tables, put
from codex_workspace import WorkspaceMixin
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import ResourceRef, StateResource


class EntityCommitPublisherTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.hub = ResourceHub("workspace-a")
        register_resource_hub(self.root, self.hub)
        self.subscription = self.hub.subscribe(
            [ResourceRef(StateResource(kind="state"))],
            loop=asyncio.get_running_loop(),
        )
        self.database = connect(self.root / "canvas.sqlite3", site="test.entity-write")
        ensure_tables(self.database)

    async def asyncTearDown(self) -> None:
        self.subscription.close()
        self.hub.close()
        self.database.close()
        unregister_resource_hub(self.root, self.hub)
        self.temporary.cleanup()

    async def assert_entity_event(self, sequences: list[int]) -> None:
        event = await self.subscription.next_event(timeout=1)
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event.resources, [ResourceRef(StateResource(kind="state"))])
        self.assertEqual(event.resourceVersions[0].revision, sequences[-1])
        self.assertEqual(event.resourceVersions[0].entitySequences, sequences)

    async def test_context_commit_savepoint_release_and_rollback(self) -> None:
        with self.database:
            put(self.database, "project", "project-a", {"id": "project-a", "name": "A"})
        await self.assert_entity_event([1])

        self.database.execute("SAVEPOINT outer_write")
        put(self.database, "project", "project-b", {"id": "project-b", "name": "B"})
        self.database.execute("RELEASE outer_write")
        await self.assert_entity_event([2])

        with self.assertRaisesRegex(RuntimeError, "rollback fixture"):
            with self.database:
                put(self.database, "project", "project-c", {"id": "project-c", "name": "C"})
                raise RuntimeError("rollback fixture")
        self.assertIsNone(await self.subscription.next_event(timeout=0.08))

    async def test_transcript_rows_do_not_publish_state_and_autocommit_does(self) -> None:
        with self.database:
            self.database.execute(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('transcript:agent-a','item:a',1,'hash',NULL,0)"
            )
        self.assertIsNone(await self.subscription.next_event(timeout=0.08))

        autocommit = connect(
            self.root / "canvas.sqlite3",
            site="test.entity-autocommit",
            isolation_level=None,
        )
        try:
            put(autocommit, "project", "project-a", {"id": "project-a", "name": "A"})
            await self.assert_entity_event([2])
        finally:
            autocommit.close()

    async def test_autocommit_executemany_publishes_entity_rows(self) -> None:
        autocommit = connect(
            self.root / "canvas.sqlite3",
            site="test.entity-autocommit-many",
            isolation_level=None,
        )
        try:
            autocommit.executemany(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('project',?,?,?,NULL,0)",
                [("project-a", 1, "a"), ("project-b", 2, "b")],
            )
            await self.assert_entity_event([1, 2])
        finally:
            autocommit.close()

    async def test_executemany_and_executescript_writes_notify_after_commit(self) -> None:
        with self.database:
            self.database.executemany(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('project',?,?,?,NULL,0)",
                [("project-a", 1, "a"), ("project-b", 2, "b")],
            )
        await self.assert_entity_event([1, 2])

        self.database.executescript(
            "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
            "VALUES ('project','project-c',3,'c',NULL,0);"
        )
        await self.assert_entity_event([3])

    async def test_raw_sqlite_writer_is_reconciled_by_the_next_in_process_commit(self) -> None:
        raw = sqlite3.connect(self.root / "canvas.sqlite3")
        try:
            raw.execute(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('project','raw-project',1,'raw',NULL,0)"
            )
            raw.commit()
        finally:
            raw.close()
        self.assertIsNone(await self.subscription.next_event(timeout=0.08))

        with self.database:
            put(self.database, "project", "local-project", {"id": "local-project", "name": "Local"})
        await self.assert_entity_event([1, 2])

    async def test_canvas_workspace_rules_and_budget_writers_share_commit_notification(self) -> None:
        canvas = Canvas(self.root)
        canvas.register_agent("host-root", "Lead")
        await self.assert_entity_event([1])
        canvas.create_chat("Planning", [], str(uuid4()))
        await self.assert_entity_event([2])

        with self.database:
            self.database.execute(
                "CREATE TABLE runtime_projects(id TEXT PRIMARY KEY, record TEXT NOT NULL)"
            )
            self.database.execute(
                "INSERT INTO runtime_projects VALUES (?,?)",
                ("/workspace/remove", json.dumps({"id": "/workspace/remove"})),
            )

        class Workspace(WorkspaceMixin):
            lock = threading.RLock()

            @contextmanager
            def db(_self):
                with self.database as connection:
                    yield connection

            @staticmethod
            def project_room_ids(_db, _path):
                return []

            @staticmethod
            def sync_agent_rooms(_db, _rooms):
                return None

        Workspace().projects({"action": "remove", "path": "/workspace/remove"})
        await self.assert_entity_event([3])

        with self.database:
            self.database.execute(
                "CREATE TABLE runtime_rules(id TEXT PRIMARY KEY, record TEXT NOT NULL)"
            )
            self.database.execute(
                "INSERT INTO runtime_rules VALUES (?,?)",
                ("rule-a", json.dumps({"id": "rule-a", "agent": "host-root"})),
            )

        class Rules(RulesMixin):
            lock = threading.RLock()

            @contextmanager
            def db(_self):
                with self.database as connection:
                    yield connection

            @staticmethod
            def checked_actor(_db, actor, _requested_actor=None):
                return {"id": actor}

            @staticmethod
            def records(_db, collection):
                return [{"id": "rule-a", "agent": "host-root"}] if collection == "rules" else []

        Rules().rules_action({"id": "rule-a", "action": "delete"}, actor="host-root")
        await self.assert_entity_event([4])

        with self.database:
            self.database.execute(
                "CREATE TABLE runtime_budget(id TEXT PRIMARY KEY, record TEXT NOT NULL)"
            )
            self.database.execute(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)"
            )
            self.database.execute(
                "INSERT INTO runtime_agents VALUES (?,?)",
                ("host-root", json.dumps({"id": "host-root", "tokensUsed": 0})),
            )
            put(
                self.database,
                "agent",
                "host-root",
                {"id": "host-root", "name": "Lead", "source": "registered", "kind": "agent"},
            )
        await self.assert_entity_event([5])
        agent = {"id": "host-root", "tokensUsed": 0}
        state = {
            "spent": 0,
            "floor": 0,
            "after": 1,
            "before": 0,
            "noticeSpent": 0,
            "historicalNotices": 0,
            "faults": False,
            "ambiguousNotices": False,
        }
        with self.database:
            save_budget(self.database, agent, state)
        await self.assert_entity_event([6])


if __name__ == "__main__":
    unittest.main()
