#!/usr/bin/env python3
"""Token-rate resource publication follows actual telemetry changes."""
import asyncio
import threading
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
sys.path.insert(0, str(ROOT / "src"))

from server_test_waits import HANG_GUARD_SECONDS
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from codex_token_rate import TokenRates
from studio_api.sync.resources import hub as resource_hub
from studio_api.sync.resources.hub import ResourceHub, register_resource_hub, unregister_resource_hub
from studio_api.sync.resources.models import (
    PanelResource,
    ResourceRef,
    ResourceTokenRatesEvent,
    TokenRateSnapshot,
)


class TokenRateEventContracts(unittest.IsolatedAsyncioTestCase):
    async def test_first_runtime_observation_seeds_pre_registered_hub(self):
        with tempfile.TemporaryDirectory() as directory:
            loop = asyncio.get_running_loop()
            hub = ResourceHub("workspace-a", token_rates=TokenRateSnapshot(rates={}, teams={}))
            register_resource_hub(directory, hub)
            subscription = hub.subscribe(
                [ResourceRef(PanelResource(kind="panel", agentId="agent-a"))],
                loop=loop,
            )
            try:
                self.assertEqual(subscription.initial_token_rates.rates, {})
                # ResourceHub is registered before Runtime is constructed in
                # make_server. First live telemetry must update its baseline.
                rates = TokenRates(clock=lambda: 100.0, state_dir=directory)
                agent = {"id": "agent-a", "rootId": "agent-a", "threadId": "thread-a",
                         "inFlight": True, "turnId": "turn-a"}
                rates.observe(agent, "turn/started", {"turnId": "turn-a"},
                              "account-a", "connection-a", 100.0)
                event = await subscription.next_event(timeout=1)
                self.assertIsInstance(event, ResourceTokenRatesEvent)
                assert isinstance(event, ResourceTokenRatesEvent)
                self.assertEqual(event.rates["agent-a"].turnId, "turn-a")
                self.assertFalse(event.rates["agent-a"].estimated)
            finally:
                subscription.close()
                unregister_resource_hub(directory, hub)

    async def test_blocked_publish_cannot_leave_hub_at_an_older_rate_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            loop = asyncio.get_running_loop()
            hub = ResourceHub("workspace-a", token_rates=TokenRateSnapshot(rates={}, teams={}))
            register_resource_hub(directory, hub)
            subscription = hub.subscribe(
                [ResourceRef(PanelResource(kind="panel", agentId="agent-a"))],
                loop=loop,
            )
            rates = TokenRates(clock=lambda: 100.0, state_dir=directory)
            first_publish_entered = threading.Event()
            release_first_publish = threading.Event()
            second_mutation_ready = threading.Event()
            second_publish_done = threading.Event()
            publish_count_lock = threading.Lock()
            publish_count = [0]
            published_outputs = []
            lock_free = []
            original_publisher = resource_hub.publish_token_rates

            def delayed_publisher(state_dir, snapshot):
                acquired = rates.lock.acquire(blocking=False)
                lock_free.append(acquired)
                if acquired:
                    rates.lock.release()
                with publish_count_lock:
                    publish_count[0] += 1
                    ordinal = publish_count[0]
                if ordinal == 1:
                    first_publish_entered.set()
                    if not release_first_publish.wait(HANG_GUARD_SECONDS):
                        raise TimeoutError("test did not release first token-rate publication")
                published_outputs.append(snapshot.rates["agent-a"].outputTokens)
                original_publisher(state_dir, snapshot)
                if ordinal == 2:
                    second_publish_done.set()

            original_after_mutation = rates._publish_if_changed
            mutation_count = [0]
            mutation_count_lock = threading.Lock()

            def mark_second_mutation():
                with mutation_count_lock:
                    mutation_count[0] += 1
                    if mutation_count[0] == 2:
                        second_mutation_ready.set()
                original_after_mutation()

            rates._publish_if_changed = mark_second_mutation
            agent = {"id": "agent-a", "rootId": "agent-a", "threadId": "thread-a",
                     "inFlight": True, "turnId": "turn-a"}
            first = threading.Thread(
                target=lambda: rates.observe(agent, "turn/started", {"turnId": "turn-a"},
                                             "account-a", "connection-a", 100.0),
            )
            second = threading.Thread(
                target=lambda: rates.stream("item/agentMessage/delta",
                                            {"threadId": "thread-a", "turnId": "turn-a",
                                             "delta": "x" * 40},
                                            "account-a", "connection-a", 102.0),
            )
            try:
                with patch.object(resource_hub, "publish_token_rates", side_effect=delayed_publisher):
                    first.start()
                    self.assertTrue(await asyncio.to_thread(first_publish_entered.wait, 1))
                    second.start()
                    self.assertTrue(await asyncio.to_thread(second_mutation_ready.wait, 1))
                    self.assertTrue(rates.lock.acquire(blocking=False))
                    rates.lock.release()
                    release_first_publish.set()
                    await asyncio.to_thread(first.join, 2)
                    await asyncio.to_thread(second.join, 2)
                    self.assertTrue(
                        await asyncio.to_thread(second_publish_done.wait, HANG_GUARD_SECONDS),
                        "coalesced publication did not publish the newer snapshot",
                    )
                self.assertEqual(published_outputs, [0, 10])
                self.assertEqual(lock_free, [True, True])
                event = await subscription.next_event(timeout=1)
                self.assertIsInstance(event, ResourceTokenRatesEvent)
                assert isinstance(event, ResourceTokenRatesEvent)
                self.assertEqual(event.rates["agent-a"].outputTokens, 10)
                self.assertEqual(rates.snapshot("agent-a")["outputTokens"], 10)
            finally:
                release_first_publish.set()
                subscription.close()
                unregister_resource_hub(directory, hub)

    async def test_changed_snapshots_publish_after_rate_lock_release(self):
        with tempfile.TemporaryDirectory() as directory:
            loop = asyncio.get_running_loop()
            hub = ResourceHub("workspace-a", token_rates=TokenRateSnapshot(rates={}, teams={}))
            register_resource_hub(directory, hub)
            subscription = hub.subscribe(
                [ResourceRef(PanelResource(kind="panel", agentId="agent-a"))],
                loop=loop,
            )
            rates = TokenRates(clock=lambda: 100.0, state_dir=directory)
            publications = []
            publication_condition = threading.Condition()
            lock_free = []
            original_publisher = resource_hub.publish_token_rates

            def capture(state_dir, snapshot):
                acquired = rates.lock.acquire(blocking=False)
                lock_free.append(acquired)
                if acquired:
                    rates.lock.release()
                original_publisher(state_dir, snapshot)
                with publication_condition:
                    publications.append(snapshot)
                    publication_condition.notify_all()

            async def wait_for_publications(count):
                def wait():
                    with publication_condition:
                        return publication_condition.wait_for(
                            lambda: len(publications) >= count,
                            timeout=HANG_GUARD_SECONDS,
                        )

                self.assertTrue(await asyncio.to_thread(wait), f"publication {count} did not arrive")

            agent = {"id": "agent-a", "rootId": "agent-a", "threadId": "thread-a",
                     "inFlight": True, "turnId": "turn-a"}
            try:
                with patch.object(resource_hub, "publish_token_rates", side_effect=capture):
                    rates.observe(agent, "turn/started", {"turnId": "turn-a"},
                                  "account-a", "connection-a", 100.0)
                    await wait_for_publications(1)
                    self.assertEqual(publications[0].rates["agent-a"].outputTokens, 0)

                    rates.stream(
                        "item/agentMessage/delta",
                        {"threadId": "thread-a", "turnId": "turn-a", "delta": "x" * 40},
                        "account-a",
                        "connection-a",
                        102.0,
                    )
                    await wait_for_publications(2)
                    self.assertTrue(publications[1].rates["agent-a"].estimated)

                    rates.observe(agent, "turn/completed", {"turnId": "turn-a"},
                                  "account-a", "connection-a", 103.0)
                    await wait_for_publications(3)
                    self.assertFalse(publications[2].rates["agent-a"].active)

                    self.assertEqual(len(publications), 3)
                    self.assertEqual(lock_free, [True, True, True])
            finally:
                for timer in rates.expiry_timers.values():
                    timer.cancel()
                subscription.close()
                unregister_resource_hub(directory, hub)


if __name__ == "__main__":
    unittest.main()
