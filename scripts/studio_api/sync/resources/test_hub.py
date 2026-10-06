"""Concurrency and lifecycle contracts for typed resource fanout."""

from __future__ import annotations

import asyncio
from collections.abc import Callable
from pathlib import Path
import tempfile
import unittest

from studio_api.sync.resources.hub import (
    MAX_ENTITY_SEQUENCE_IDS,
    MAX_PENDING_RESOURCES,
    MAX_RESOURCE_REVISION_ENTRIES,
    ResourceHub,
    publish_resources,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    AccountsResource,
    CostsResource,
    LimitsResource,
    PanelResource,
    QueueResource,
    ResourceRef,
    ResourceTokenRatesEvent,
    StateResource,
    TokenRateSnapshot,
    TokenRateValue,
)


def panel(agent_id: str) -> ResourceRef:
    return ResourceRef(PanelResource(kind="panel", agentId=agent_id))


class Watchdog:
    def __init__(self, callback_immediately: bool = False) -> None:
        self.callback_immediately = callback_immediately
        self.attached: list[str] = []
        self.detached: list[str] = []
        self.callbacks: dict[str, Callable[[], None]] = {}

    def subscribe(self, agent_id: str, on_change: Callable[[], None]) -> Callable[[], None]:
        self.attached.append(agent_id)
        self.callbacks[agent_id] = on_change
        if self.callback_immediately:
            on_change()

        def detach() -> None:
            self.detached.append(agent_id)

        return detach


class CrossThreadWatchdog:
    """Model native callbacks that can run concurrently with attach/detach."""

    def __init__(self) -> None:
        self.callback: Callable[[], None] | None = None

    @staticmethod
    def _run_callback(callback: Callable[[], None]) -> None:
        import threading

        completed = threading.Event()
        thread = threading.Thread(target=lambda: (callback(), completed.set()), daemon=True)
        thread.start()
        if not completed.wait(1):
            raise AssertionError("hub lock was held while entering watchdog lifecycle")
        thread.join(timeout=1)

    def subscribe(self, agent_id: str, on_change: Callable[[], None]) -> Callable[[], None]:
        self.callback = on_change
        self._run_callback(on_change)

        def detach() -> None:
            assert self.callback is not None
            self._run_callback(self.callback)

        return detach


class ResourceHubTests(unittest.IsolatedAsyncioTestCase):
    async def test_resource_revision_survives_subscription_gap_and_is_bounded(self) -> None:
        loop = asyncio.get_running_loop()
        costs = ResourceRef(CostsResource(kind="costs"))
        hub = ResourceHub("workspace-revisions")
        first = hub.subscribe([costs], loop=loop)
        first.close()
        hub.publish(costs)
        hub.publish(ResourceRef(AccountsResource(kind="accounts")))

        returned = hub.subscribe([costs], loop=loop)
        self.assertEqual(returned.initial.resourceVersions[0].revision, 1)
        returned.close()

        resources = [
            ResourceRef(LimitsResource(kind="limits", accountKey=f"account-{index}"))
            for index in range(MAX_RESOURCE_REVISION_ENTRIES + 4)
        ]
        hub.publish_many(resources)
        self.assertLessEqual(len(hub._resource_revisions), MAX_RESOURCE_REVISION_ENTRIES)
        hub.close()

    async def test_state_entity_sequence_is_independent_and_commit_bursts_coalesce(self) -> None:
        loop = asyncio.get_running_loop()
        state = ResourceRef(StateResource(kind="state"))
        watched_panel = panel("agent-state-test")
        hub = ResourceHub("workspace-state", entity_sequence=4)
        subscription = hub.subscribe([state, watched_panel], loop=loop)
        initial = {
            resource.root.kind: version.revision
            for resource, version in zip(
                subscription.initial.resources,
                subscription.initial.resourceVersions,
                strict=True,
            )
        }
        self.assertEqual(initial["state"], 4)
        self.assertEqual(initial["panel"], 0)

        hub.publish(watched_panel)
        typed = await subscription.next_event(timeout=1)
        self.assertIsNotNone(typed)
        assert typed is not None
        typed_versions = {
            resource.root.kind: version.revision
            for resource, version in zip(typed.resources, typed.resourceVersions, strict=True)
        }
        self.assertEqual(typed_versions["panel"], 1)

        hub.publish_entity_sequence(5, [5])
        hub.publish_entity_sequence(6, [6])
        state_event = await subscription.next_event(timeout=1)
        self.assertIsNotNone(state_event)
        assert state_event is not None
        self.assertEqual(state_event.resources, [state])
        self.assertEqual(state_event.resourceVersions[0].revision, 6)
        self.assertEqual(state_event.resourceVersions[0].entitySequences, [5, 6])
        self.assertIsNone(await subscription.next_event(timeout=0.05))

        hub.publish_overflow()
        overflow = await subscription.next_event(timeout=1)
        self.assertIsNotNone(overflow)
        assert overflow is not None
        state_version = next(
            version.revision
            for resource, version in zip(
                overflow.resources, overflow.resourceVersions, strict=True
            )
            if resource.root.kind == "state"
        )
        self.assertEqual(state_version, 6)
        subscription.close()

    async def test_entity_sequence_payload_is_bounded_and_signals_reset(self) -> None:
        loop = asyncio.get_running_loop()
        state = ResourceRef(StateResource(kind="state"))
        hub = ResourceHub("workspace-state-bounded")
        subscription = hub.subscribe([state], loop=loop)

        hub.publish_entity_sequence(
            MAX_ENTITY_SEQUENCE_IDS + 1,
            list(range(1, MAX_ENTITY_SEQUENCE_IDS + 2)),
        )
        event = await subscription.next_event(timeout=1)

        self.assertIsNotNone(event)
        assert event is not None
        version = event.resourceVersions[0]
        self.assertIsNone(version.entitySequences)
        self.assertTrue(version.entitySequenceReset)
        subscription.close()

    async def test_entity_sequence_without_frame_does_not_advance_hub_revision(self) -> None:
        loop = asyncio.get_running_loop()
        state = ResourceRef(StateResource(kind="state"))
        hub = ResourceHub("workspace-sequence-no-op", entity_sequence=10)
        subscription = hub.subscribe([state], loop=loop)

        self.assertEqual(hub.publish_entity_sequence(20), 0)
        self.assertEqual(hub._resource_revision(state), 10)

        hub.publish_entity_sequence(11, [11])
        event = await subscription.next_event(timeout=1)
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event.resourceVersions[0].revision, 11)
        self.assertEqual(event.resourceVersions[0].entitySequences, [11])
        subscription.close()

    async def test_atomic_baseline_and_change_coalescing(self) -> None:
        loop = asyncio.get_running_loop()
        hub = ResourceHub("workspace-a")
        subscription = hub.subscribe([panel("agent-a")], loop=loop)
        self.assertEqual(subscription.initial.reason, "initial")
        self.assertEqual(subscription.initial.revision, 0)
        hub.publish(panel("agent-a"))
        hub.publish(panel("agent-a"))
        event = await subscription.next_event(timeout=1)
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event.reason, "change")
        self.assertEqual(event.revision, 2)
        self.assertEqual(event.resources, [panel("agent-a")])
        self.assertIsNone(await subscription.next_event(timeout=0.01))
        self.assertEqual(subscription.heartbeat().revision, 2)
        subscription.close()

    async def test_progress_watch_is_refcounted_and_baselines_immediate_callback(self) -> None:
        loop = asyncio.get_running_loop()
        watchdog = Watchdog(callback_immediately=True)
        hub = ResourceHub("workspace-a", watchdog)
        first = hub.subscribe([panel("agent-a")], loop=loop)
        second = hub.subscribe([panel("agent-a")], loop=loop)
        self.assertEqual(watchdog.attached, ["agent-a", "agent-a"])
        self.assertEqual(first.initial.revision, 1)
        self.assertEqual(second.initial.revision, 2)
        changed = await first.next_event(timeout=1)
        self.assertIsNotNone(changed)
        self.assertEqual(changed.revision, 2)
        self.assertIsNone(await second.next_event(timeout=0.01))
        first.close()
        self.assertEqual(watchdog.detached, ["agent-a"])
        second.close()
        self.assertEqual(watchdog.detached, ["agent-a", "agent-a"])

    async def test_watchdog_attach_and_detach_never_run_under_hub_lock(self) -> None:
        loop = asyncio.get_running_loop()
        watchdog = CrossThreadWatchdog()
        hub = ResourceHub("workspace-a", watchdog)
        subscription = hub.subscribe([panel("agent-a")], loop=loop)
        self.assertEqual(subscription.initial.revision, 1)
        self.assertIsNone(await subscription.next_event(timeout=0.01))
        subscription.close()

    async def test_hub_close_detaches_all_live_subscriptions_idempotently(self) -> None:
        loop = asyncio.get_running_loop()
        watchdog = Watchdog()
        hub = ResourceHub("workspace-a", watchdog)
        first = hub.subscribe([panel("agent-a")], loop=loop)
        second = hub.subscribe([panel("agent-a")], loop=loop)
        hub.close()
        hub.close()
        first.close()
        second.close()
        self.assertEqual(watchdog.detached, ["agent-a", "agent-a"])

    async def test_publish_after_loop_close_detaches_orphaned_subscription(self) -> None:
        stale_loop = asyncio.new_event_loop()
        watchdog = Watchdog()
        hub = ResourceHub("workspace-a", watchdog)
        hub.subscribe([panel("agent-a")], loop=stale_loop)
        stale_loop.close()
        hub.publish(panel("agent-a"))
        self.assertEqual(len(hub._subscriptions), 0)
        self.assertEqual(watchdog.detached, ["agent-a"])
        hub.close()
        self.assertEqual(watchdog.detached, ["agent-a"])

    async def test_overflow_reconciles_full_subscription(self) -> None:
        loop = asyncio.get_running_loop()
        resources = [
            ResourceRef(QueueResource(kind="queue", agentId=f"agent-{index}"))
            for index in range(MAX_PENDING_RESOURCES + 1)
        ]
        hub = ResourceHub("workspace-a")
        subscription = hub.subscribe(resources, loop=loop)
        hub.publish_many(resources)
        event = await subscription.next_event(timeout=1)
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event.reason, "overflow")
        actual = {resource.model_dump_json() for resource in event.resources}
        expected = {resource.model_dump_json() for resource in resources}
        self.assertEqual(actual, expected)
        subscription.close()

    async def test_registry_publication_is_safe_without_and_with_a_hub(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            hub = ResourceHub("workspace-a")
            publish_resources(directory, panel("agent-a"))
            register_resource_hub(directory, hub)
            self.assertEqual(hub.publish_many(()), 0)
            self.assertEqual(publish_resources(directory, panel("agent-a")), None)
            unregister_resource_hub(directory, hub)
            publish_resources(Path(directory), panel("agent-a"))
            self.assertEqual(hub.publish_many(()), 1)

    async def test_token_rate_changes_are_typed_coalesced_and_deduplicated(self) -> None:
        loop = asyncio.get_running_loop()
        snapshot = TokenRateSnapshot(rates={}, teams={})
        hub = ResourceHub("workspace-a", token_rates=snapshot)
        subscription = hub.subscribe([panel("agent-a")], loop=loop)
        self.assertEqual(subscription.initial_token_rates.rates, {})
        changed = TokenRateSnapshot(
            rates={"agent-a": TokenRateValue(
                turnId="turn-a", active=True, estimated=False, rate=4.5, outputTokens=9,
            )},
            teams={},
        )
        self.assertEqual(hub.publish_token_rates(changed), 1)
        self.assertEqual(hub.publish_token_rates(changed), 1)
        event = await subscription.next_event(timeout=1)
        self.assertIsInstance(event, ResourceTokenRatesEvent)
        assert isinstance(event, ResourceTokenRatesEvent)
        self.assertEqual(event.rates["agent-a"].outputTokens, 9)
        self.assertIsNone(await subscription.next_event(timeout=0.01))
        subscription.close()


if __name__ == "__main__":
    unittest.main()
