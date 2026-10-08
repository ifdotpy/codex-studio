"""Every supported instrumented commit publishes its entity sequence once."""

from __future__ import annotations

import asyncio
from collections.abc import Iterator
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import time
import unittest
from typing import cast
from unittest.mock import patch
from uuid import uuid4

from codex_budget import _save as save_budget
from codex_canvas import Canvas
from codex_records import AgentRecord, BudgetStateRecord, JsonObject
from codex_rules import RulesMixin, RulesRuntime
from codex_sqlite import connect
from codex_sync_entities import ensure_tables, put
from codex_workspace import WorkspaceMixin
from studio_api.sync.resources.hub import (
    ENTITY_SEQUENCE_THROTTLE_SECONDS,
    MAX_ENTITY_SEQUENCE_DELAY_SECONDS,
    EntityPublicationScheduler,
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import ResourceChangeEvent, ResourceRef, StateResource


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
        self.assertEqual(
            self.database.execute("PRAGMA journal_mode=WAL").fetchone()[0],
            "wal",
        )
        from studio_api.sync.resources import hub as resources_hub

        self.initial_scan = threading.Event()
        scheduler = resources_hub._entity_publication_scheduler
        original_publish = scheduler._publish

        def observe_initial_scan(
            root: str,
            database_path: Path,
            due_at: float,
            deadline: float,
            sequence: int,
            entity_sequences: set[int],
            reset: bool,
        ) -> None:
            original_publish(
                root,
                database_path,
                due_at,
                deadline,
                sequence,
                entity_sequences,
                reset,
            )
            if root == str(self.root):
                self.initial_scan.set()

        self.initial_scan_patch = patch.object(
            scheduler,
            "_publish",
            side_effect=observe_initial_scan,
        )
        self.initial_scan_patch.start()
        ensure_tables(self.database)
        self.assertTrue(await asyncio.to_thread(self.initial_scan.wait, 5))
        self.initial_scan_patch.stop()
        # Drain schema/setup commits before each test starts observing writes.
        await asyncio.sleep(ENTITY_SEQUENCE_THROTTLE_SECONDS + 0.05)
        while await self.subscription.next_event(timeout=0.01) is not None:
            pass

    async def asyncTearDown(self) -> None:
        self.initial_scan_patch.stop()
        self.subscription.close()
        self.hub.close()
        self.database.close()
        unregister_resource_hub(self.root, self.hub)
        self.temporary.cleanup()

    async def assert_entity_event(self, sequences: list[int]) -> None:
        event = await self.subscription.next_event(timeout=1)
        self.assertIsNotNone(event)
        assert isinstance(event, ResourceChangeEvent)
        self.assertEqual(event.resources, [ResourceRef(StateResource(kind="state"))])
        self.assertEqual(event.resourceVersions[0].revision, sequences[-1])
        self.assertEqual(event.resourceVersions[0].entitySequences, sequences)

    async def test_context_commit_savepoint_release_and_rollback(self) -> None:
        with self.database:
            self.database.execute("BEGIN IMMEDIATE")
            put(self.database, "project", "project-a", {"id": "project-a", "name": "A"})
        await self.assert_entity_event([1])

        self.database.execute("SAVEPOINT outer_write")
        put(self.database, "project", "project-b", {"id": "project-b", "name": "B"})
        self.database.execute("RELEASE outer_write")
        await self.assert_entity_event([2])

        with self.assertRaisesRegex(RuntimeError, "rollback fixture"):
            with self.database:
                self.database.execute("BEGIN IMMEDIATE")
                put(self.database, "project", "project-c", {"id": "project-c", "name": "C"})
                raise RuntimeError("rollback fixture")
        self.assertIsNone(await self.subscription.next_event(timeout=0.08))

    async def test_transcript_rows_do_not_publish_state_and_entity_put_does(self) -> None:
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
            autocommit.execute("BEGIN IMMEDIATE")
            put(autocommit, "project", "project-a", {"id": "project-a", "name": "A"})
            autocommit.commit()
            await self.assert_entity_event([2])
        finally:
            autocommit.close()

    async def test_non_transcript_checkpoint_scan_uses_partial_seq_index(self) -> None:
        with self.database:
            self.database.executemany(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('transcript:agent-a',?,?,?,NULL,0)",
                [(f"item-{index}", index + 10, str(index)) for index in range(300_000)],
            )
            self.database.executemany(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('project',?,?,?,NULL,0)",
                [(f"project-{index}", index + 1, str(index)) for index in range(20)],
            )
        plan = self.database.execute(
            "EXPLAIN QUERY PLAN SELECT COALESCE(MAX(seq),0) FROM sync_entities "
            "WHERE collection NOT LIKE 'transcript:%'"
        ).fetchall()
        started = time.perf_counter()
        maximum = self.database.execute(
            "SELECT COALESCE(MAX(seq),0) FROM sync_entities "
            "WHERE collection NOT LIKE 'transcript:%'",
        ).fetchone()[0]
        elapsed_ms = (time.perf_counter() - started) * 1000
        self.assertEqual(maximum, 20)
        print({"checkpointRows": 300_000, "plan": plan, "elapsedMs": round(elapsed_ms, 3)})
        self.assertIn(
            "sync_entities_non_transcript_seq",
            " ".join(str(row) for row in plan),
        )
        self.assertLess(elapsed_ms, 250)

    async def test_restarted_hub_reuses_entity_sequence_in_a_new_epoch(self) -> None:
        state = ResourceRef(StateResource(kind="state"))
        with self.database:
            self.database.execute("BEGIN IMMEDIATE")
            put(self.database, "project", "before-restart", {"id": "before-restart"})
        await self.assert_entity_event([1])
        previous_epoch = self.hub.epoch
        self.subscription.close()

        restarted = ResourceHub("workspace-a", entity_sequence=1)
        register_resource_hub(self.root, restarted)
        subscription = restarted.subscribe([state], loop=asyncio.get_running_loop())
        try:
            self.assertNotEqual(subscription.initial.epoch, previous_epoch)
            self.assertEqual(subscription.initial.resourceVersions[0].revision, 1)
            with self.database:
                self.database.execute("BEGIN IMMEDIATE")
                put(self.database, "project", "after-restart", {"id": "after-restart"})
            event = await subscription.next_event(timeout=1)
            self.assertIsNotNone(event)
            assert isinstance(event, ResourceChangeEvent)
            self.assertEqual(event.resourceVersions[0].revision, 2)
            self.assertEqual(event.resourceVersions[0].entitySequences, [2])
        finally:
            subscription.close()
            restarted.close()
            unregister_resource_hub(self.root, restarted)

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

    async def test_commit_succeeds_when_publication_handoff_raises(self) -> None:
        with patch(
            "studio_api.sync.resources.hub.schedule_entity_publication",
            side_effect=RuntimeError("scheduler unavailable"),
        ):
            with self.database:
                self.database.execute("BEGIN IMMEDIATE")
                put(self.database, "project", "committed", {"id": "committed"})

        row = self.database.execute(
            "SELECT seq FROM sync_entities WHERE collection='project' AND id='committed'"
        ).fetchone()
        self.assertEqual(row, (1,))

    async def test_scheduler_recovers_after_one_publication_failure(self) -> None:
        from studio_api.sync.resources import hub as resources_hub

        scheduler = resources_hub._entity_publication_scheduler
        original_publish = scheduler._publish
        failed = threading.Event()
        calls = 0

        def fail_once(*args: object) -> None:
            nonlocal calls
            calls += 1
            if calls == 1:
                failed.set()
                raise RuntimeError("transient publication failure")
            original_publish(*args)  # type: ignore[arg-type]

        with patch.object(scheduler, "_publish", side_effect=fail_once):
            with self.database:
                self.database.execute("BEGIN IMMEDIATE")
                put(self.database, "project", "first", {"id": "first"})
            self.assertTrue(await asyncio.to_thread(failed.wait, 5))
            await self.assert_entity_event([1])
        self.assertGreaterEqual(calls, 2)

    async def test_scheduler_retries_with_capped_backoff_and_quiet_logging(self) -> None:
        clock = [0.0]
        scheduler = EntityPublicationScheduler(
            clock=lambda: clock[0], start_worker=False
        )
        attempts: list[float] = []

        def fail_eight_times(
            _root: str,
            _database_path: Path,
            _due_at: float,
            _deadline: float,
            _sequence: int,
            _ids: set[int],
            _reset: bool,
        ) -> None:
            attempts.append(clock[0])
            if len(attempts) <= 8:
                raise RuntimeError("transient publication failure")

        scheduler.schedule(self.root, self.root / "canvas.sqlite3", 1, (1,))
        clock[0] = ENTITY_SEQUENCE_THROTTLE_SECONDS
        with self.assertLogs("studio_api.sync.resources.hub", level="WARNING") as logs:
            scheduler._publish = fail_eight_times  # type: ignore[assignment]
            scheduler.flush_due()
            for delay in (0.25, 0.5, 1.0, 2.0, 4.0, 5.0, 5.0, 5.0):
                clock[0] += delay
                scheduler.flush_due()

        self.assertEqual(len(attempts), 9)
        self.assertEqual(
            attempts,
            [0.1, 0.35, 0.85, 1.85, 3.85, 7.85, 12.85, 17.85, 22.85],
        )
        self.assertEqual(len(logs.records), 8)
        self.assertIsNotNone(logs.records[0].exc_info)
        self.assertTrue(all(record.exc_info is None for record in logs.records[1:]))
        self.assertEqual(scheduler._pending, {})

    async def test_publication_scan_includes_unlisted_entity_collections(self) -> None:
        with self.database:
            # Model a collection added to the storage contract after this
            # publisher was written; the current DTO projector correctly
            # rejects unknown collections, so seed the committed row directly.
            self.database.execute(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('futureCollection','future-1',1,'digest',?,0)",
                ('{"collection":"futureCollection","id":"future-1","value":{}}',),
            )

        EntityPublicationScheduler._publish(
            str(self.root),
            self.root / "canvas.sqlite3",
            0.0,
            1.0,
            0,
            set(),
            False,
        )
        event = await self.subscription.next_event(timeout=1)
        self.assertIsNotNone(event)
        assert isinstance(event, ResourceChangeEvent)
        self.assertEqual(event.resourceVersions[0].entitySequences, [1])

    async def test_scheduler_coalesces_with_a_controlled_clock(self) -> None:
        clock = [0.0]
        scheduler = EntityPublicationScheduler(
            clock=lambda: clock[0], start_worker=False
        )
        database_path = self.root / "canvas.sqlite3"
        publications: list[tuple[str, Path, float, float, int, set[int], bool]] = []

        def capture(
            root: str,
            database_path: Path,
            due_at: float,
            deadline: float,
            sequence: int,
            entity_sequences: set[int],
            reset: bool,
        ) -> None:
            publications.append(
                (
                    root,
                    database_path,
                    due_at,
                    deadline,
                    sequence,
                    entity_sequences,
                    reset,
                )
            )

        with patch.object(scheduler, "_publish", side_effect=capture):
            # All 30 commits land inside one 100 ms quiet window.
            for sequence in range(1, 31):
                clock[0] = (sequence - 1) * (0.09 / 29)
                scheduler.schedule(
                    self.root, database_path, sequence, (sequence,)
                )
                self.assertEqual(scheduler.flush_due(), 0)
            clock[0] += ENTITY_SEQUENCE_THROTTLE_SECONDS
            self.assertEqual(scheduler.flush_due(), 1)

        self.assertEqual(len(publications), 1)
        self.assertEqual(publications[0][4], 30)
        self.assertEqual(publications[0][5], set(range(1, 31)))
        tight_count = len(publications)

        clock[0] = 0.0
        scheduler = EntityPublicationScheduler(
            clock=lambda: clock[0], start_worker=False
        )
        publications = []
        with patch.object(scheduler, "_publish", side_effect=capture):
            for sequence in range(1, 31):
                clock[0] = (sequence - 1) * (2.0 / 29)
                scheduler.flush_due()
                scheduler.schedule(
                    self.root, database_path, sequence, (sequence,)
                )
            clock[0] += ENTITY_SEQUENCE_THROTTLE_SECONDS
            scheduler.flush_due()

        maximum = 21  # ceil(2000 ms / 100 ms) + one trailing publication.
        self.assertGreaterEqual(len(publications), 4)
        self.assertLessEqual(len(publications), maximum)
        self.assertEqual(publications[-1][4], 30)
        self.assertIn(30, publications[-1][5])
        print(
            "CONTROLLED_COMMIT_COALESCING",
            {
                "inside100ms": {"commits": 30, "publications": tight_count},
                "spread2s": {
                    "commits": 30,
                    "publications": len(publications),
                    "allowed": [4, maximum],
                    "finalSequence": publications[-1][4],
                },
            },
        )

    async def test_continuous_commits_publish_during_burst_with_bounded_latency(self) -> None:
        from studio_api.sync.resources import hub as resources_hub

        clock = [0.0]
        scheduler = EntityPublicationScheduler(clock=lambda: clock[0], start_worker=False)
        original_scheduler = resources_hub._entity_publication_scheduler
        original_publish = scheduler._publish
        publication_times: list[float] = []

        def publish(
            root: str,
            database_path: Path,
            _due_at: float,
            _deadline: float,
            explicit_sequence: int,
            explicit_sequences: set[int],
            explicit_reset: bool,
        ) -> None:
            publication_times.append(clock[0])
            original_publish(
                root,
                database_path,
                _due_at,
                _deadline,
                explicit_sequence,
                explicit_sequences,
                explicit_reset,
            )

        scheduler._publish = publish  # type: ignore[method-assign]
        resources_hub._entity_publication_scheduler = scheduler
        events: list[ResourceChangeEvent] = []
        try:
            for index in range(40):
                clock[0] = index * 0.05
                previous_count = len(publication_times)
                with self.database:
                    self.database.execute("BEGIN IMMEDIATE")
                    put(
                        self.database,
                        "agent",
                        f"continuous-agent-{index}",
                        {"id": f"continuous-agent-{index}", "kind": "agent",
                         "tokensUsed": index + 1},
                    )
                scheduler.flush_due()
                if len(publication_times) > previous_count:
                    event = await self.subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
                    assert isinstance(event, ResourceChangeEvent)
                    events.append(event)

            # The final quiet-window event may trail the continuous burst, but
            # no pending sequence can exceed the scheduler's 500 ms cap.
            clock[0] += ENTITY_SEQUENCE_THROTTLE_SECONDS
            scheduler.flush_due()
            if len(events) < len(publication_times):
                event = await self.subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert isinstance(event, ResourceChangeEvent)
                events.append(event)
        finally:
            resources_hub._entity_publication_scheduler = original_scheduler

        self.assertEqual(len(events), len(publication_times))
        self.assertGreaterEqual(len(events), 3)
        self.assertLess(publication_times[0], 1.95, "must publish before the 40-commit burst ends")
        observed_sequences: set[int] = set()
        for event, published_at in zip(events, publication_times):
            versions = event.resourceVersions[0]
            sequences = versions.entitySequences or []
            self.assertTrue(sequences)
            first_sequence = min(sequences)
            first_commit_at = (first_sequence - 1) * 0.05
            self.assertLessEqual(published_at - first_commit_at, MAX_ENTITY_SEQUENCE_DELAY_SECONDS)
            self.assertEqual(event.resources, [ResourceRef(StateResource(kind="state"))])
            self.assertEqual(versions.revision, max(versions.entitySequences or []))
            observed_sequences.update(sequences)
        self.assertEqual(observed_sequences, set(range(1, 41)))
        self.assertEqual(events[-1].resourceVersions[0].revision, 40)

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

        with self.assertRaises(sqlite3.OperationalError):
            self.database.executescript(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                "VALUES ('project','project-partial',4,'partial',NULL,0); "
                "INSERT INTO missing_table VALUES (1);"
            )
        await self.assert_entity_event([4])

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
            self.database.execute("BEGIN IMMEDIATE")
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
            def db(_self) -> Iterator[sqlite3.Connection]:
                with self.database as connection:
                    yield connection

            @staticmethod
            def project_room_ids(_db: sqlite3.Connection, _path: str) -> list[str]:
                return []

            @staticmethod
            def sync_agent_rooms(_db: sqlite3.Connection, _rooms: list[str]) -> None:
                return None

        Workspace().projects(  # type: ignore[misc]
            {"action": "remove", "path": "/workspace/remove"}
        )
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
            def db(_self) -> Iterator[sqlite3.Connection]:
                with self.database as connection:
                    yield connection

            @staticmethod
            def checked_actor(
                _db: sqlite3.Connection,
                actor: str | None,
                _requested_actor: str | None = None,
            ) -> JsonObject:
                return {"id": actor}

            @staticmethod
            def records(_db: sqlite3.Connection, collection: str) -> list[JsonObject]:
                return [{"id": "rule-a", "agent": "host-root"}] if collection == "rules" else []

        cast(RulesRuntime, Rules()).rules_action(
            {"id": "rule-a", "action": "delete"}, actor="host-root"
        )
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
        agent: AgentRecord = {
            "id": "host-root",
            "rootId": "host-root",
            "epoch": 0,
            "status": "idle",
            "tokensUsed": 0,
        }
        state: BudgetStateRecord = {
            "cutoff": 0.0,
            "floor": 0,
            "spent": 0,
            "before": 0,
            "after": 1,
            "historicalNotices": 0,
            "noticeSpent": 0,
            "counter": None,
            "thread": None,
            "noticeAt": 0,
            "created": 0.0,
            "lastAmount": None,
            "ambiguousNotices": [],
            "faults": [],
        }
        state["noticeSpent"] = 13
        with self.database:
            save_budget(self.database, agent, state)
        await self.assert_entity_event([6])


if __name__ == "__main__":
    unittest.main()
