"""Integration contracts for runtime changes entering the resource hub."""

from __future__ import annotations

import asyncio
from pathlib import Path
from queue import SimpleQueue
import tempfile
import threading
import unittest

from codex_runtime import Runtime
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    LimitsResource,
    ResourceRef,
    ResourceTokenRatesEvent,
    TokenRateSnapshot,
    TokenRateValue,
)


class RuntimeResourcePublisherTests(unittest.IsolatedAsyncioTestCase):
    async def test_limits_emit_after_commit_only_for_visible_projection_change(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            runtime = Runtime.__new__(Runtime)
            runtime.root = state_dir
            runtime.db_path = state_dir / "runtime.sqlite"
            runtime.analytics_db_path = state_dir / "analytics.sqlite"
            runtime.lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime._committed_resource_changes = SimpleQueue()
            runtime._committed_resource_callbacks = SimpleQueue()
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
                        runtime._stage_resource_callback(
                            db,
                            lambda: hub.publish_token_rates(TokenRateSnapshot(
                                rates={"agent-a": TokenRateValue(
                                    turnId="turn-a", active=True, estimated=False,
                                    rate=2, outputTokens=4,
                                )},
                                teams={},
                            )),
                        )
                        raise RuntimeError("rollback")
                runtime._publish_committed_resource_changes()
                self.assertIsNone(await token_subscription.next_event(timeout=0.01))

                with runtime.db() as db:
                    runtime._stage_resource_callback(
                        db,
                        lambda: hub.publish_token_rates(TokenRateSnapshot(
                            rates={"agent-a": TokenRateValue(
                                turnId="turn-a", active=True, estimated=False,
                                rate=2, outputTokens=4,
                            )},
                            teams={},
                        )),
                    )
                    self.assertIsNone(await token_subscription.next_event(timeout=0.01))
                runtime._publish_committed_resource_changes()
                token_event = await token_subscription.next_event(timeout=1)
                self.assertIsInstance(token_event, ResourceTokenRatesEvent)
                token_subscription.close()
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)
                hub.close()


if __name__ == "__main__":
    unittest.main()
