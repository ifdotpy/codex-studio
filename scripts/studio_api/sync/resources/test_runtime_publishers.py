"""Integration contracts for runtime changes entering the resource hub."""

from __future__ import annotations

import asyncio
import json
from contextlib import contextmanager
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch

from codex_runtime import (
    MAX_STAGED_RESOURCE_CHANGES,
    Runtime,
    TokenRateObservation,
    workspace_agent_resource_changed,
)
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    LimitsResource,
    PanelResource,
    QueueResource,
    ReceiptsResource,
    ResourceRef,
    StateResource,
    TasksResource,
)


class RuntimeResourcePublisherTests(unittest.IsolatedAsyncioTestCase):
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
                detached = threading.Event()
                writer_locked = threading.Event()
                release_writer = threading.Event()
                captured: list[tuple[ResourceRef, ...]] = []
                resource_lock = threading.Lock()

                class DrainLock:
                    def __enter__(self):
                        resource_lock.acquire()
                        return self

                    def __exit__(self, *_args: object) -> None:
                        resource_lock.release()
                        if threading.current_thread().name == "resource-publisher":
                            detached.set()

                runtime._committed_resource_lock = DrainLock()
                first = ResourceRef(StateResource(kind="state"))
                second = ResourceRef(LimitsResource(kind="limits", accountKey="account-b"))
                runtime._committed_resource_changes = {first.model_dump_json(by_alias=True): first}
                runtime._committed_resource_overflow = False

                def writer() -> None:
                    self.assertTrue(detached.wait(1))
                    with getattr(runtime, barrier_name):
                        writer_locked.set()
                        key = second.model_dump_json(by_alias=True)
                        with runtime._committed_resource_lock:
                            runtime._committed_resource_changes[key] = second
                        self.assertTrue(release_writer.wait(1))

                with patch("studio_api.sync.resources.hub.publish_resources", side_effect=lambda _root, *refs: captured.append(refs)):
                    publisher = threading.Thread(target=runtime._publish_committed_resource_changes, name="resource-publisher")
                    writer_thread = threading.Thread(target=writer, name="resource-writer")
                    publisher.start()
                    writer_thread.start()
                    self.assertTrue(writer_locked.wait(1))
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
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = {}
            runtime._committed_resource_overflow = False
            runtime._committed_resource_lock = threading.Lock()
            local = threading.local()
            runtime._callback_db = local
            connection = sqlite3.connect(":memory:")
            connection.execute("CREATE TABLE runtime_agents(id TEXT, record TEXT)")
            connection.execute("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT)")
            for agent_id, root_id in (("lead", "lead"), ("worker-a", "lead"), ("worker-b", "lead")):
                connection.execute(
                    "INSERT INTO runtime_agents VALUES (?,?)",
                    (agent_id, json.dumps({"rootId": root_id})),
                )
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
            try:
                with patch("codex_sync_entities.put"), patch("codex_sync_entities.sync_task_write"):
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
            finally:
                for subscription in subscriptions:
                    subscription.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()
                connection.close()

    async def test_event_mutation_stages_queue_and_receipt_refs(self) -> None:
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
            def database():
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
