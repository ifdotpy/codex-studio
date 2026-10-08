"""Integration contracts for runtime changes entering the resource hub."""

from __future__ import annotations

import asyncio
import json
from contextlib import contextmanager
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from codex_runtime import (
    AppServer,
    MAX_STAGED_RESOURCE_CHANGES,
    Runtime,
    transcript_agent_resource_changed,
    TokenRateObservation,
    workspace_agent_resource_changed,
)
from codex_sync_entities import ensure_tables
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    DesktopResource,
    LimitsResource,
    PanelResource,
    QueueResource,
    ReceiptsResource,
    ResourceRef,
    StateResource,
    TranscriptResource,
    TasksResource,
)


class RuntimeResourcePublisherTests(unittest.IsolatedAsyncioTestCase):
    def test_transcript_projection_changes_publish_but_maintenance_writes_stay_quiet(self) -> None:
        before = {
            "id": "agent-a", "status": "running", "activity": {"phase": "tool"},
            "inFlight": True, "contextUsage": {"used": 10}, "autoWake": True,
            "turnId": "turn-a", "threadId": "thread-a", "updated": 1,
        }
        self.assertTrue(transcript_agent_resource_changed(before, {**before, "status": "interrupted"}))
        self.assertFalse(transcript_agent_resource_changed(before, {**before, "updated": 2, "tokensUsed": 9}))

    async def test_agent_put_publishes_transcript_status_only_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime.ui_condition = threading.Condition(runtime.lock)
            runtime.ui_revisions = {}
            runtime._rate_cache_lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            local = threading.local()
            runtime._callback_db = local
            connection = sqlite3.connect(":memory:")
            connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)")
            prior = {
                "id": "agent-a", "name": "Worker", "status": "running",
                "activity": {"phase": "tool"}, "inFlight": True, "autoWake": True,
                "threadId": "thread-a", "turnId": "turn-a", "rootId": "agent-a",
            }
            connection.execute("INSERT INTO runtime_agents VALUES (?,?)", ("agent-a", json.dumps(prior)))
            runtime.agent_entity_view = lambda _db, record: record
            runtime.chat_rooms = lambda *_args, **_kwargs: []
            runtime.mark_agent_records_changed = lambda *_args, **_kwargs: None
            staged = {connection: {}}
            overflowed = {connection: False}
            local.after_commit_resources = staged
            local.after_commit_resource_overflow = overflowed

            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(TranscriptResource(kind="transcript", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            try:
                with (
                    patch("codex_sync_entities.put"),
                    patch("codex_sync_entities.sync_task_agent_change"),
                    patch("codex_sync_entities.sync_monitor_agent_change"),
                    patch("codex_execution.needs_record", return_value=False),
                ):
                    runtime.put(connection, "agents", {**prior, "status": "interrupted"})
                runtime._queue_staged_resource_changes(staged, overflowed, connection)
                runtime._publish_committed_resource_changes()
                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)

                for terminal_status in ("failed", "paused"):
                    current = json.loads(connection.execute(
                        "SELECT record FROM runtime_agents WHERE id='agent-a'"
                    ).fetchone()[0])
                    staged[connection] = {}
                    with patch("codex_sync_entities.put"):
                        with patch("codex_execution.needs_record", return_value=False):
                            runtime.put(connection, "agents", {**current, "status": terminal_status})
                    runtime._queue_staged_resource_changes(staged, overflowed, connection)
                    runtime._publish_committed_resource_changes()
                    self.assertIsNotNone(await subscription.next_event(timeout=1), terminal_status)

                staged[connection] = {}
                current = json.loads(connection.execute(
                    "SELECT record FROM runtime_agents WHERE id='agent-a'"
                ).fetchone()[0])
                with (
                    patch("codex_sync_entities.put"),
                    patch("codex_execution.needs_record", return_value=False),
                ):
                    runtime.put(connection, "agents", {
                        **current, "updated": 2, "tokensUsed": 99,
                    })
                runtime._queue_staged_resource_changes(staged, overflowed, connection)
                runtime._publish_committed_resource_changes()
                self.assertIsNone(await subscription.next_event(timeout=0.02))
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()
                connection.close()

    async def test_disconnect_without_stream_buffer_invalidates_transcript(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime._rate_cache_lock = threading.RLock()
            runtime.ui_condition = threading.Condition(runtime.lock)
            runtime.ui_revisions = {}
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            runtime.connection_ids = {"default": "connection-a"}
            runtime.offline_accounts = set()
            runtime.offline = False
            runtime.loaded = {"agent-a"}
            runtime.servers = {}
            runtime.preparations = {}
            local = threading.local()
            runtime._callback_db = local
            connection = sqlite3.connect(":memory:")
            connection.row_factory = sqlite3.Row
            connection.executescript("""
                CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT);
                CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, kind TEXT, text TEXT,
                    status TEXT, error TEXT, created REAL, epoch INTEGER, turn_id TEXT);
                CREATE TABLE runtime_tasks(record TEXT);
                CREATE TABLE runtime_monitors(record TEXT);
            """)
            agent = {
                "id": "agent-a", "rootId": "agent-a", "status": "running",
                "autoWake": True, "inFlight": True, "threadId": "thread-a", "turnId": "turn-a",
                "epoch": 1, "accountKey": "default", "name": "Worker",
            }
            connection.execute("INSERT INTO runtime_agents VALUES (?,?)", ("agent-a", json.dumps(agent)))

            @contextmanager
            def database(*, busy_timeout=None):
                staged = {connection: {}}
                overflow = {connection: False}
                local.after_commit_resources = staged
                local.after_commit_resource_overflow = overflow
                try:
                    yield connection
                    connection.commit()
                    runtime._queue_staged_resource_changes(staged, overflow, connection)
                except BaseException:
                    connection.rollback()
                    raise

            runtime.db = database
            runtime.records = lambda _db, table=None: [agent] if table == "agents" else []
            runtime.retire_legacy_steer = lambda *_args: None
            runtime.capacity_restart = lambda *_args: None
            runtime.mark_agent_records_changed = lambda *_args: None
            runtime.agent_entity_view = lambda _db, record: record
            runtime.chat_rooms = lambda *_args, **_kwargs: []
            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            transcript = hub.subscribe(
                [ResourceRef(TranscriptResource(kind="transcript", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            try:
                with (
                    patch("codex_sync_entities.put"),
                    patch("codex_sync_entities.sync_task_agent_change"),
                    patch("codex_sync_entities.sync_monitor_agent_change"),
                    patch("codex_execution.needs_record", return_value=False),
                ):
                    runtime.disconnected("default", "connection-a")
                runtime._publish_committed_resource_changes()
                event = await transcript.next_event(timeout=1)
                self.assertIsNotNone(event)
                self.assertEqual(
                    event.resources,
                    [ResourceRef(TranscriptResource(kind="transcript", agentId="agent-a"))],
                )
                stored = json.loads(connection.execute(
                    "SELECT record FROM runtime_agents WHERE id='agent-a'"
                ).fetchone()[0])
                self.assertEqual(stored["status"], "interrupted")
            finally:
                transcript.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()
                connection.close()

    def test_agent_telemetry_does_not_invalidate_workspace_projection(self) -> None:
        before = {
            "id": "agent-a", "status": "running", "tokensUsed": 10,
            "updated": 1, "activity": {"phase": "thinking", "at": 1},
        }
        telemetry = {
            **before, "tokensUsed": 11, "updated": 2,
            "activity": {"phase": "thinking", "at": 2},
        }
        self.assertFalse(workspace_agent_resource_changed(before, telemetry))
        self.assertTrue(workspace_agent_resource_changed(before, {**telemetry, "status": "completed"}))
        self.assertTrue(workspace_agent_resource_changed(None, before))

    def test_token_observation_batch_flushes_in_order_after_outer_lock(self) -> None:
        runtime = Runtime.__new__(Runtime)
        runtime.lock = threading.RLock()
        local_calls: list[TokenRateObservation] = []
        runtime._publish_token_rate_observation = local_calls.append
        observations = [
            TokenRateObservation(
                agent={"id": "agent-a", "threadId": "thread-a"},
                method="turn/started",
                params={"turn": {"id": "turn-a"}},
                account_key="default", connection_id="connection-a", observed_at=1,
            ),
            TokenRateObservation(
                agent={"id": "agent-a", "threadId": "thread-a", "inFlight": True},
                method="item/started",
                params={"turnId": "turn-a", "item": {"id": "item-a", "type": "agentMessage"}},
                account_key="default", connection_id="connection-a", observed_at=2,
            ),
        ]

        with runtime._token_rate_observation_batch():
            with runtime.lock:
                for observation in observations:
                    runtime._dispatch_token_rate_observation(observation)
                self.assertEqual(local_calls, [])
        self.assertEqual(local_calls, observations)

    def test_token_delta_after_committed_turn_start_is_not_waiting_for_scheduler(self) -> None:
        from codex_token_rate import TokenRates

        rates = TokenRates(clock=lambda: 3)
        agent = {"id": "agent-a", "rootId": "agent-a", "threadId": "thread-a"}
        rates.observe(agent, "turn/started", {"turn": {"id": "turn-a"}}, "default", "connection-a", 1)
        rates.stream("item/agentMessage/delta", {
            "threadId": "thread-a", "turnId": "turn-a", "delta": "hello there",
        }, "default", "connection-a", 2)
        snapshot = rates.workspace_snapshot()
        self.assertTrue(snapshot["rates"]["agent-a"]["active"])
        self.assertGreater(snapshot["rates"]["agent-a"]["outputTokens"], 0)

    def test_resource_drain_detaches_before_waiting_on_writer_locks(self) -> None:
        for barrier_name in ("lock", "_rate_cache_lock"):
            with self.subTest(barrier=barrier_name):
                runtime = Runtime.__new__(Runtime)
                runtime.root = Path(".")
                runtime.lock = threading.RLock()
                runtime._rate_cache_lock = threading.RLock()
                runtime.changed = threading.Event()
                publisher_waiting = threading.Event()
                writer_locked = threading.Event()
                writer_queued = threading.Event()
                release_writer = threading.Event()
                captured: list[tuple[ResourceRef, ...]] = []
                resource_lock = threading.Lock()

                class BarrierLock:
                    def __enter__(self):
                        if threading.current_thread().name == "resource-publisher":
                            publisher_waiting.set()
                        resource_lock.acquire()
                        return self

                    def __exit__(self, *_args: object) -> None:
                        resource_lock.release()

                runtime._committed_resource_lock = threading.Lock()
                barrier = BarrierLock()
                setattr(runtime, barrier_name, barrier)
                first = ResourceRef(StateResource(kind="state"))
                second = ResourceRef(LimitsResource(kind="limits", accountKey="account-b"))
                runtime._committed_resource_changes = {first.model_dump_json(by_alias=True): first}
                runtime._committed_resource_overflow = False

                def writer() -> None:
                    with barrier:
                        writer_locked.set()
                        self.assertTrue(publisher_waiting.wait(1))
                        key = second.model_dump_json(by_alias=True)
                        with runtime._committed_resource_lock:
                            runtime._committed_resource_changes[key] = second
                        writer_queued.set()
                        self.assertTrue(release_writer.wait(1))

                with patch("studio_api.sync.resources.hub.publish_resources", side_effect=lambda _root, *refs: captured.append(refs)):
                    publisher = threading.Thread(target=runtime._publish_committed_resource_changes, name="resource-publisher")
                    writer_thread = threading.Thread(target=writer, name="resource-writer")
                    writer_thread.start()
                    self.assertTrue(writer_locked.wait(1))
                    publisher.start()
                    self.assertTrue(writer_queued.wait(1))
                    self.assertEqual(captured, [])
                    release_writer.set()
                    publisher.join(timeout=1)
                    writer_thread.join(timeout=1)
                    self.assertFalse(publisher.is_alive())
                    self.assertFalse(writer_thread.is_alive())
                    self.assertEqual(captured, [(first,)])
                    self.assertEqual(list(runtime._committed_resource_changes.values()), [second])
                    runtime._publish_committed_resource_changes()
                    self.assertEqual(captured, [(first,), (second,)])

    async def test_task_change_invalidates_feed_for_every_team_member(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime._rate_cache_lock = threading.RLock()
            runtime.ui_condition = threading.Condition(runtime.lock)
            runtime.ui_revisions = {}
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            local = threading.local()
            runtime._callback_db = local
            connection = sqlite3.connect(":memory:")
            connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)")
            connection.execute("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT)")
            connection.execute("CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY, record TEXT)")
            connection.execute("CREATE TABLE runtime_federation_rooms(id TEXT PRIMARY KEY, record TEXT)")
            for agent_id, root_id in (("lead", "lead"), ("worker-a", "lead"), ("worker-b", "lead")):
                connection.execute(
                    "INSERT INTO runtime_agents VALUES (?,?)",
                    (agent_id, json.dumps({"id": agent_id, "rootId": root_id})),
                )
            runtime.agent_entity_view = lambda _db, record: record
            runtime.chat_rooms = lambda *_args, **_kwargs: []
            runtime.mark_agent_records_changed = lambda *_args, **_kwargs: None
            resources: dict[object, dict[str, ResourceRef]] = {connection: {}}
            overflowed: dict[object, bool] = {connection: False}
            local.after_commit_resources = resources
            local.after_commit_resource_overflow = overflowed

            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            subscriptions = [
                hub.subscribe(
                    [ResourceRef(TasksResource(kind="tasks", agentId=agent_id))],
                    loop=asyncio.get_running_loop(),
                )
                for agent_id in ("lead", "worker-a", "worker-b")
            ]
            transcript_subscription = hub.subscribe(
                [ResourceRef(TranscriptResource(kind="transcript", agentId="worker-b"))],
                loop=asyncio.get_running_loop(),
            )
            worker_transcript_subscription = hub.subscribe(
                [ResourceRef(TranscriptResource(kind="transcript", agentId="worker-a"))],
                loop=asyncio.get_running_loop(),
            )
            try:
                with (
                    patch("codex_sync_entities.put"),
                    patch("codex_sync_entities.sync_task_write"),
                    patch("codex_sync_entities.sync_task_agent_change"),
                    patch("codex_sync_entities.sync_monitor_agent_change"),
                ):
                    runtime.put(connection, "tasks", {
                        "id": "task-a", "agent": "worker-a", "status": "running",
                    })
                runtime._queue_staged_resource_changes(resources, overflowed, connection)
                runtime._publish_committed_resource_changes()
                for subscription, agent_id in zip(subscriptions, ("lead", "worker-a", "worker-b")):
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event, agent_id)
                    assert event is not None
                    self.assertEqual(
                        event.resources,
                        [ResourceRef(TasksResource(kind="tasks", agentId=agent_id))],
                    )
                # The transcript endpoint derives linked toolStatus from task records.
                self.assertIsNotNone(await worker_transcript_subscription.next_event(timeout=1))
                with (
                    patch("codex_sync_entities.put"),
                    patch("codex_sync_entities.sync_task_agent_change"),
                    patch("codex_sync_entities.sync_monitor_agent_change"),
                ):
                    runtime.put(connection, "agents", {
                        "id": "worker-b", "rootId": "lead", "deletedAt": 10,
                    })
                runtime._queue_staged_resource_changes(resources, overflowed, connection)
                runtime._publish_committed_resource_changes()
                for subscription in subscriptions[:2]:
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
                self.assertIsNone(await subscriptions[2].next_event(timeout=0.01))
                self.assertIsNotNone(await transcript_subscription.next_event(timeout=1))
            finally:
                for subscription in subscriptions:
                    subscription.close()
                transcript_subscription.close()
                worker_transcript_subscription.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()
                connection.close()

    async def test_event_mutation_stages_queue_and_receipt_refs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime.ui_condition = threading.Condition(runtime.lock)
            runtime._rate_cache_lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            local = threading.local()
            runtime._callback_db = local
            connection = object()
            resources: dict[object, dict[str, ResourceRef]] = {connection: {}}
            overflowed: dict[object, bool] = {connection: False}
            local.after_commit_resources = resources
            local.after_commit_resource_overflow = overflowed

            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            queue = hub.subscribe(
                [ResourceRef(QueueResource(kind="queue", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            receipts = hub.subscribe(
                [ResourceRef(ReceiptsResource(kind="receipts", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            try:
                runtime._stage_event_resources(connection, "agent-a")
                runtime._queue_staged_resource_changes(resources, overflowed, connection)
                runtime._publish_committed_resource_changes()
                for subscription in (queue, receipts):
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
                    assert event is not None
                    self.assertEqual(len(event.resources), 1)
                self.assertIsNone(await queue.next_event(timeout=0.01))
                self.assertIsNone(await receipts.next_event(timeout=0.01))
            finally:
                queue.close()
                receipts.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()

    async def test_queue_cancel_commits_both_queue_and_receipt_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime.ui_condition = threading.Condition(runtime.lock)
            runtime._rate_cache_lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            local = threading.local()
            runtime._callback_db = local
            connection = sqlite3.connect(":memory:")
            connection.row_factory = sqlite3.Row
            connection.executescript("""
                CREATE TABLE runtime_events(
                    id TEXT PRIMARY KEY, agent TEXT, kind TEXT, text TEXT, status TEXT,
                    error TEXT, created REAL, epoch INTEGER, turn_id TEXT
                );
                CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY, record TEXT);
                CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY, signature TEXT, result TEXT);
                INSERT INTO runtime_events VALUES ('event-a','agent-a','user','hello','pending',NULL,1,1,NULL);
                INSERT INTO runtime_event_meta VALUES ('event-a','{}');
            """)

            @contextmanager
            def database(*, busy_timeout=None):
                staged: dict[object, dict[str, ResourceRef]] = {connection: {}}
                overflow: dict[object, bool] = {connection: False}
                local.after_commit_resources = staged
                local.after_commit_resource_overflow = overflow
                try:
                    yield connection
                    connection.commit()
                    runtime._queue_staged_resource_changes(staged, overflow, connection)
                except BaseException:
                    connection.rollback()
                    raise

            runtime.db = database
            runtime.agent = lambda agent_id, _db: {
                "id": agent_id, "epoch": 1, "queueMutationRevision": 0,
            }
            runtime.put = lambda *_args, **_kwargs: None
            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            queue = hub.subscribe(
                [ResourceRef(QueueResource(kind="queue", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            receipts = hub.subscribe(
                [ResourceRef(ReceiptsResource(kind="receipts", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            try:
                runtime.queue_action("agent-a", {
                    "action": "cancel", "message_id": "event-a", "request_id": "cancel-a",
                })
                runtime._publish_committed_resource_changes()
                for subscription, kind in ((queue, "queue"), (receipts, "receipts")):
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
                    assert event is not None
                    self.assertEqual(event.resources[0].root.kind, kind)
                runtime.queue_action("agent-a", {
                    "action": "cancel", "message_id": "event-a", "request_id": "cancel-a",
                })
                runtime._publish_committed_resource_changes()
                self.assertIsNone(await queue.next_event(timeout=0.01))
                self.assertIsNone(await receipts.next_event(timeout=0.01))
                self.assertEqual(
                    connection.execute("SELECT status FROM runtime_events WHERE id='event-a'").fetchone()[0],
                    "cancelled",
                )
            finally:
                queue.close()
                receipts.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()
                connection.close()

    async def test_scheduler_publishes_committed_queue_and_receipt_refs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime._rate_cache_lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime.changed.set()
            runtime.closed = False
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            local = threading.local()
            runtime._callback_db = local
            connection = object()
            staged = {connection: {}}
            overflowed = {connection: False}
            local.after_commit_resources = staged
            local.after_commit_resource_overflow = overflowed

            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            queue = hub.subscribe(
                [ResourceRef(QueueResource(kind="queue", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            receipts = hub.subscribe(
                [ResourceRef(ReceiptsResource(kind="receipts", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            runtime._retry_dirty_workspace_refresh = lambda: None
            runtime.monitors_tick = lambda: None
            runtime.rules_tick = lambda: None
            runtime.capacity_tick = lambda: None
            runtime.usage_resume_tick = lambda: None
            runtime.dispatch = lambda: setattr(runtime, "closed", True)
            try:
                runtime._stage_event_resources(connection, "agent-a")
                runtime._queue_staged_resource_changes(staged, overflowed, connection)
                runtime.schedule()
                for subscription in (queue, receipts):
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
            finally:
                queue.close()
                receipts.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()

    async def test_runtime_event_sql_writes_are_commit_only_and_deduped(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.db_path = state_dir / "canvas.sqlite3"
            runtime.analytics_db_path = state_dir / "analytics.sqlite3"
            runtime.lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            runtime._agent_records_cache_lock = threading.RLock()
            runtime.analytics_safe = lambda *_args, **_kwargs: None
            runtime.schedule_analytics_captures = lambda *_args, **_kwargs: None
            runtime.mark_agent_records_changed = lambda *_args, **_kwargs: None
            seed = sqlite3.connect(runtime.db_path)
            try:
                seed.execute("""CREATE TABLE runtime_events(
                    id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                    text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                    epoch INTEGER NOT NULL, turn_id TEXT, error TEXT
                )""")
                seed.execute(
                    "INSERT INTO runtime_events VALUES ('event-a','agent-a','user','hello','pending',1,1,NULL,NULL)"
                )
                seed.commit()
            finally:
                seed.close()

            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            queue = hub.subscribe(
                [ResourceRef(QueueResource(kind="queue", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            receipts = hub.subscribe(
                [ResourceRef(ReceiptsResource(kind="receipts", agentId="agent-a"))],
                loop=asyncio.get_running_loop(),
            )
            try:
                with self.assertRaisesRegex(RuntimeError, "rollback"):
                    with runtime.db() as db:
                        db.execute("UPDATE runtime_events SET status='reserved' WHERE id='event-a'")
                        raise RuntimeError("rollback")
                runtime._publish_committed_resource_changes()
                self.assertIsNone(await queue.next_event(timeout=0.01))
                self.assertIsNone(await receipts.next_event(timeout=0.01))

                with runtime.db() as db:
                    db.execute("UPDATE runtime_events SET status='reserved' WHERE id='event-a'")
                runtime._publish_committed_resource_changes()
                for subscription in (queue, receipts):
                    self.assertIsNotNone(await subscription.next_event(timeout=1))

                with runtime.db() as db:
                    db.execute("UPDATE runtime_events SET status='reserved' WHERE id='event-a'")
                runtime._publish_committed_resource_changes()
                self.assertIsNone(await queue.next_event(timeout=0.01))
                self.assertIsNone(await receipts.next_event(timeout=0.01))

                with runtime.db() as db:
                    db.execute("DELETE FROM runtime_events WHERE id='event-a'")
                runtime._publish_committed_resource_changes()
                for subscription in (queue, receipts):
                    self.assertIsNotNone(await subscription.next_event(timeout=1))

                with runtime.db() as db:
                    db.execute(
                        "INSERT INTO runtime_events VALUES "
                        "('event-b','agent-a','user','again','pending',2,1,NULL,NULL)"
                    )
                runtime._publish_committed_resource_changes()
                for subscription in (queue, receipts):
                    self.assertIsNotNone(await subscription.next_event(timeout=1))

                with runtime.db() as db:
                    db.execute("UPDATE runtime_events SET status='failed' WHERE id='event-b'")
                runtime._publish_committed_resource_changes()
                for subscription in (queue, receipts):
                    self.assertIsNotNone(await subscription.next_event(timeout=1))

                with runtime.db() as db:
                    db.execute("UPDATE runtime_events SET status='cancelled' WHERE id='event-b'")
                runtime._publish_committed_resource_changes()
                for subscription in (queue, receipts):
                    self.assertIsNotNone(await subscription.next_event(timeout=1))
            finally:
                queue.close()
                receipts.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()

    async def test_separate_supervisor_process_exit_reaches_desktop_subscriber(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            child = state_dir / "fake-app-server"
            child.write_text(
                "#!/usr/bin/env python3\n"
                "import json,sys\n"
                "request=json.loads(sys.stdin.readline())\n"
                "print(json.dumps({'id':request['id'],'result':{}}),flush=True)\n"
                "sys.stdin.readline()\n"
            )
            child.chmod(0o700)
            supervisor_script = Path(__file__).resolve().parents[3] / "codex_process_supervisor.py"
            repo_root = Path(__file__).resolve().parents[4]
            environment = dict(os.environ)
            environment["CODEX_AGENTS_SUPERVISOR_MODE"] = "1"
            environment["CODEX_AGENTS_STATE_DIR"] = str(state_dir)
            supervisor = subprocess.Popen(
                [sys.executable, str(supervisor_script), "--state", str(state_dir)],
                cwd=repo_root, env=environment, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            server = None
            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(DesktopResource(kind="desktop"))],
                loop=asyncio.get_running_loop(),
            )
            disconnected = threading.Event()
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime.ui_condition = threading.Condition(runtime.lock)
            runtime.ui_revisions = {}
            runtime._rate_cache_lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            runtime.closed = False
            runtime.connection_ids = {"default": "connection-a"}
            runtime.offline_accounts = set()
            runtime.offline = False
            runtime.loaded = set()
            runtime.servers = {}
            runtime.preparations = {}
            runtime.records = lambda _db, _table: []
            db_connection = sqlite3.connect(":memory:", check_same_thread=False)
            db_connection.row_factory = sqlite3.Row
            db_connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)")
            db_connection.execute("CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT)")
            db_connection.execute("CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY, record TEXT)")
            db_connection.execute("CREATE TABLE runtime_federation_rooms(id TEXT PRIMARY KEY, record TEXT)")
            db_connection.execute("CREATE TABLE runtime_tasks(record TEXT)")
            db_connection.execute("CREATE TABLE runtime_monitors(record TEXT)")
            db_connection.execute(
                "CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL, "
                "text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL, epoch INTEGER NOT NULL, "
                "turn_id TEXT, error TEXT)"
            )
            db_connection.execute("INSERT INTO runtime_agents VALUES (?,?)", (
                "agent-a", json.dumps({"id": "agent-a", "accountKey": "default", "status": "idle"})))
            ensure_tables(db_connection)

            @contextmanager
            def database(*, busy_timeout=None):
                yield db_connection

            runtime.db = database

            def on_disconnect() -> None:
                runtime.disconnected("default", "connection-a")
                disconnected.set()

            try:
                deadline = time.monotonic() + 10
                socket_path = state_dir / "supervisor.sock"
                while not socket_path.exists() and supervisor.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(socket_path.exists(), "separate supervisor did not create its IPC socket")
                account_root = state_dir / "account-server"
                account_root.mkdir()
                with patch.dict(os.environ, environment):
                    server = AppServer(
                        account_root, lambda _message: None, lambda _message: None,
                        on_disconnect, executable=str(child), supervisor_handle="account:default",
                        supervisor_root=state_dir,
                    )
                self.assertTrue(disconnected.wait(10), "supervisor exit did not reach Runtime.disconnected")
                runtime._publish_committed_resource_changes()
                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(event.resources, [ResourceRef(DesktopResource(kind="desktop"))])
            finally:
                if server is not None:
                    server.close()
                subscription.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()
                db_connection.close()
                supervisor.terminate()
                try:
                    supervisor.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    supervisor.kill()
                    supervisor.wait(timeout=5)

    async def test_limits_emit_after_commit_only_for_visible_projection_change(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.db_path = state_dir / "runtime.sqlite"
            runtime.analytics_db_path = state_dir / "analytics.sqlite"
            runtime.lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            runtime.rate_limits = {
                "accountKey": "default", "data": None, "at": None, "error": None,
            }
            runtime.rate_limits_by_account = {}
            runtime.analytics_safe = lambda *_args, **_kwargs: None
            runtime.schedule_analytics_captures = lambda *_args, **_kwargs: None
            runtime.mark_agent_records_changed = lambda *_args, **_kwargs: None

            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(LimitsResource(kind="limits", accountKey="default"))],
                loop=asyncio.get_running_loop(),
            )
            try:
                initial = {
                    "data": {"rateLimits": {"primary": {"usedPercent": 12}}},
                    "at": 1,
                }
                self.assertTrue(runtime.store_rate_limits("default", initial))
                self.assertTrue(runtime.changed.is_set())
                runtime._publish_committed_resource_changes()
                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(event.resources, [
                    ResourceRef(LimitsResource(kind="limits", accountKey="default")),
                ])

                # Read freshness changes, but the renderer's visible limits do not.
                self.assertFalse(runtime.store_rate_limits("default", {**initial, "at": 2}))
                runtime._publish_committed_resource_changes()
                self.assertIsNone(await subscription.next_event(timeout=0.01))

                changed = {
                    **initial,
                    "at": 3,
                    "data": {"rateLimits": {"primary": {"usedPercent": 13}}},
                }
                self.assertTrue(runtime.store_rate_limits("default", changed))
                runtime._publish_committed_resource_changes()
                changed_event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(changed_event)
                assert changed_event is not None
                self.assertEqual(changed_event.resources, [
                    ResourceRef(LimitsResource(kind="limits", accountKey="default")),
                ])

                token_subscription = hub.subscribe(
                    [ResourceRef(LimitsResource(kind="limits", accountKey="default"))],
                    loop=asyncio.get_running_loop(),
                )
                with self.assertRaisesRegex(RuntimeError, "rollback"):
                    with runtime.db() as db:
                        runtime._stage_resource_change(
                            db,
                            ResourceRef(LimitsResource(kind="limits", accountKey="rolled-back")),
                        )
                        raise RuntimeError("rollback")
                runtime._publish_committed_resource_changes()
                self.assertIsNone(await token_subscription.next_event(timeout=0.01))

                with runtime.db() as db:
                    runtime._stage_resource_change(
                        db,
                        ResourceRef(LimitsResource(kind="limits", accountKey="default")),
                    )
                self.assertIsNone(await token_subscription.next_event(timeout=0.01))
                runtime._publish_committed_resource_changes()
                committed_event = await token_subscription.next_event(timeout=1)
                self.assertIsNotNone(committed_event)
                assert committed_event is not None
                self.assertEqual(committed_event.resources, [
                    ResourceRef(LimitsResource(kind="limits", accountKey="default")),
                ])
                token_subscription.close()
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()

    async def test_staged_resource_overflow_forces_full_subscription_reconcile(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.lock = threading.RLock()
            runtime._rate_cache_lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            local = threading.local()
            runtime._callback_db = local
            connection = object()
            staged: dict[object, dict[str, ResourceRef]] = {connection: {}}
            overflowed: dict[object, bool] = {connection: False}
            local.after_commit_resources = staged
            local.after_commit_resource_overflow = overflowed
            for index in range(MAX_STAGED_RESOURCE_CHANGES + 1):
                resource = ResourceRef(PanelResource(kind="panel", agentId=f"agent-{index}"))
                runtime._stage_resource_change(connection, resource)
            runtime._queue_staged_resource_changes(staged, overflowed, connection)
            self.assertEqual(len(runtime._committed_resource_changes), MAX_STAGED_RESOURCE_CHANGES)
            self.assertTrue(runtime._committed_resource_overflow)

            hub = ResourceHub("workspace-a")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(StateResource(kind="state"))], loop=asyncio.get_running_loop()
            )
            try:
                runtime._publish_committed_resource_changes()
                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(event.reason, "overflow")
                self.assertEqual(event.resources, [ResourceRef(StateResource(kind="state"))])
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()


if __name__ == "__main__":
    unittest.main()
