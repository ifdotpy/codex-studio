"""Concurrency and lifecycle contracts for typed resource fanout."""

from __future__ import annotations

import asyncio
from pathlib import Path
import tempfile
import unittest

# A wait for an expected event uses the hang guard; it only guards against a hang.
from server_test_waits import HANG_GUARD_SECONDS

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
    MultiServerResource,
    QueueResource,
    ResourceRef,
    ResourceTokenRatesEvent,
    StateResource,
    TokenRateSnapshot,
    TokenRateValue,
)


def queue(agent_id: str) -> ResourceRef:
    return ResourceRef(QueueResource(kind="queue", agentId=agent_id))


class ResourceHubTests(unittest.IsolatedAsyncioTestCase):
    async def test_multi_server_resource_baseline_and_publication_are_typed(self) -> None:
        loop = asyncio.get_running_loop()
        resource = ResourceRef(MultiServerResource(kind="multi-server"))
        hub = ResourceHub("workspace-multi-server")
        subscription = hub.subscribe([resource], loop=loop)

        baseline = subscription.initial.model_dump(mode="json", by_alias=True)
        self.assertEqual(baseline["resources"], [{"kind": "multi-server"}])
        self.assertEqual(baseline["resourceVersions"][0]["revision"], 0)

        self.assertEqual(hub.publish(resource), 1)
        event = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event.resources, [resource])
        published = event.model_dump(mode="json", by_alias=True)
        self.assertEqual(published["resources"], [{"kind": "multi-server"}])
        self.assertEqual(published["resourceVersions"][0]["revision"], 1)
        subscription.close()

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
        watched_queue = queue("agent-state-test")
        hub = ResourceHub("workspace-state", entity_sequence=4)
        subscription = hub.subscribe([state, watched_queue], loop=loop)
        initial = {
            resource.root.kind: version.revision
            for resource, version in zip(
                subscription.initial.resources,
                subscription.initial.resourceVersions,
                strict=True,
            )
        }
        self.assertEqual(initial["state"], 4)
        self.assertEqual(initial["queue"], 0)

        hub.publish(watched_queue)
        typed = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
        self.assertIsNotNone(typed)
        assert typed is not None
        typed_versions = {
            resource.root.kind: version.revision
            for resource, version in zip(typed.resources, typed.resourceVersions, strict=True)
        }
        self.assertEqual(typed_versions["queue"], 1)

        hub.publish_entity_sequence(5, [5])
        hub.publish_entity_sequence(6, [6])
        state_event = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
        self.assertIsNotNone(state_event)
        assert state_event is not None
        self.assertEqual(state_event.resources, [state])
        self.assertEqual(state_event.resourceVersions[0].revision, 6)
        self.assertEqual(state_event.resourceVersions[0].entitySequences, [5, 6])
        self.assertIsNone(await subscription.next_event(timeout=0.05))

        hub.publish_overflow()
        overflow = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
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
        event = await subscription.next_event(timeout=HANG_GUARD_SECONDS)

        self.assertIsNotNone(event)
        assert event is not None
        version = event.resourceVersions[0]
        self.assertIsNone(version.entitySequences)
        self.assertTrue(version.entitySequenceReset)
        subscription.close()

    async def test_reset_published_during_subscription_gap_is_in_next_baseline(self) -> None:
        loop = asyncio.get_running_loop()
        state = ResourceRef(StateResource(kind="state"))
        hub = ResourceHub("workspace-state-reset-gap", entity_sequence=12)

        first = hub.subscribe([state], loop=loop)
        second = hub.subscribe([state], loop=loop)
        self.assertFalse(first.initial.resourceVersions[0].entitySequenceReset)
        self.assertFalse(second.initial.resourceVersions[0].entitySequenceReset)
        first.close()
        second.close()

        hub.publish_entity_sequence(12, reset=True)
        reopened_a = hub.subscribe([state], loop=loop, reconnect=True)
        reopened_b = hub.subscribe([state], loop=loop, reconnect=True)

        for reopened in (reopened_a, reopened_b):
            self.assertEqual(reopened.initial.reason, "reconnect")
            self.assertEqual(reopened.initial.resourceVersions[0].revision, 12)
            self.assertTrue(reopened.initial.resourceVersions[0].entitySequenceReset)
            reopened.close()

        repeated_baseline_a = hub.subscribe([state], loop=loop, reconnect=True)
        self.assertTrue(
            repeated_baseline_a.initial.resourceVersions[0].entitySequenceReset
        )
        repeated_baseline_a.close()

        hub.publish_entity_sequence(13, [13])
        baseline_after_commit = hub.subscribe(
            [state], loop=loop, reconnect=True
        )
        self.assertFalse(
            baseline_after_commit.initial.resourceVersions[0].entitySequenceReset
        )
        baseline_after_commit.close()

    async def test_entity_sequence_without_frame_does_not_advance_hub_revision(self) -> None:
        loop = asyncio.get_running_loop()
        state = ResourceRef(StateResource(kind="state"))
        hub = ResourceHub("workspace-sequence-no-op", entity_sequence=10)
        subscription = hub.subscribe([state], loop=loop)

        self.assertEqual(hub.publish_entity_sequence(20), 0)
        self.assertEqual(hub._resource_revision(state), 10)

        hub.publish_entity_sequence(11, [11])
        event = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event.resourceVersions[0].revision, 11)
        self.assertEqual(event.resourceVersions[0].entitySequences, [11])
        subscription.close()

    async def test_atomic_baseline_and_change_coalescing(self) -> None:
        loop = asyncio.get_running_loop()
        hub = ResourceHub("workspace-a")
        subscription = hub.subscribe([queue("agent-a")], loop=loop)
        self.assertEqual(subscription.initial.reason, "initial")
        self.assertEqual(subscription.initial.revision, 0)
        hub.publish(queue("agent-a"))
        hub.publish(queue("agent-a"))
        event = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
        self.assertIsNotNone(event)
        assert event is not None
        self.assertEqual(event.reason, "change")
        self.assertEqual(event.revision, 2)
        self.assertEqual(event.resources, [queue("agent-a")])
        self.assertIsNone(await subscription.next_event(timeout=0.01))
        self.assertEqual(subscription.heartbeat().revision, 2)
        subscription.close()

    async def test_hub_close_closes_all_live_subscriptions_idempotently(self) -> None:
        loop = asyncio.get_running_loop()
        hub = ResourceHub("workspace-a")
        first = hub.subscribe([queue("agent-a")], loop=loop)
        second = hub.subscribe([queue("agent-a")], loop=loop)
        hub.close()
        hub.close()
        first.close()
        second.close()
        self.assertEqual(len(hub._subscriptions), 0)

    async def test_publish_after_loop_close_closes_orphaned_subscription(self) -> None:
        stale_loop = asyncio.new_event_loop()
        hub = ResourceHub("workspace-a")
        hub.subscribe([queue("agent-a")], loop=stale_loop)
        stale_loop.close()
        hub.publish(queue("agent-a"))
        self.assertEqual(len(hub._subscriptions), 0)
        hub.close()
        self.assertEqual(len(hub._orphaned_subscriptions), 0)

    async def test_overflow_reconciles_full_subscription(self) -> None:
        loop = asyncio.get_running_loop()
        resources = [
            ResourceRef(QueueResource(kind="queue", agentId=f"agent-{index}"))
            for index in range(MAX_PENDING_RESOURCES + 1)
        ]
        hub = ResourceHub("workspace-a")
        subscription = hub.subscribe(resources, loop=loop)
        hub.publish_many(resources)
        event = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
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
            publish_resources(directory, queue("agent-a"))
            register_resource_hub(directory, hub)
            self.assertEqual(hub.publish_many(()), 0)
            self.assertEqual(publish_resources(directory, queue("agent-a")), None)
            unregister_resource_hub(directory, hub)
            publish_resources(Path(directory), queue("agent-a"))
            self.assertEqual(hub.publish_many(()), 1)

    async def test_token_rate_changes_are_typed_coalesced_and_deduplicated(self) -> None:
        loop = asyncio.get_running_loop()
        snapshot = TokenRateSnapshot(rates={}, teams={})
        hub = ResourceHub("workspace-a", token_rates=snapshot)
        subscription = hub.subscribe([queue("agent-a")], loop=loop)
        self.assertEqual(subscription.initial_token_rates.rates, {})
        changed = TokenRateSnapshot(
            rates={"agent-a": TokenRateValue(
                turnId="turn-a", active=True, estimated=False, rate=4.5, outputTokens=9,
            )},
            teams={},
        )
        self.assertEqual(hub.publish_token_rates(changed), 1)
        self.assertEqual(hub.publish_token_rates(changed), 1)
        event = await subscription.next_event(timeout=HANG_GUARD_SECONDS)
        self.assertIsInstance(event, ResourceTokenRatesEvent)
        assert isinstance(event, ResourceTokenRatesEvent)
        self.assertEqual(event.rates["agent-a"].outputTokens, 9)
        self.assertIsNone(await subscription.next_event(timeout=0.01))
        subscription.close()


if __name__ == "__main__":
    unittest.main()
