"""Concurrency and lifecycle contracts for typed resource fanout."""

from __future__ import annotations

import asyncio
from collections.abc import Callable
from pathlib import Path
import tempfile
import unittest

from studio_api.sync.resources.hub import (
    MAX_PENDING_RESOURCES,
    ResourceHub,
    publish_resources,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    PanelResource,
    QueueResource,
    ResourceRef,
    ResourceTokenRatesEvent,
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
