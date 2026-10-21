"""Continuous commits publish at bounded wall-clock intervals.

This witness measures real time, so it belongs to the heavy tier: a busy machine
can stretch its 50 ms commit pacing. The ordinary tier owns the same coalescing
contract with a controlled clock (test_scheduler_coalesces_with_a_controlled_clock).
"""

from __future__ import annotations

import asyncio
import time
import unittest

from codex_sync_entities import put
from studio_api.sync.resources.models import ResourceChangeEvent
from studio_api.sync.resources.test_entity_commit_publisher import PublisherFixture


class EntityCommitRealTimeTests(PublisherFixture):
    async def test_continuous_commits_publish_at_bounded_intervals(self) -> None:
        events: list[tuple[float, ResourceChangeEvent]] = []
        commit_times: list[float] = []

        async def receive_until_quiet() -> None:
            while True:
                event = await self.subscription.next_event(timeout=0.8)
                if event is None:
                    return
                assert isinstance(event, ResourceChangeEvent)
                events.append((time.monotonic(), event))

        receiver = asyncio.create_task(receive_until_quiet())
        started_at = time.monotonic()
        for index in range(40):
            next_commit_at = started_at + index * 0.05
            await asyncio.sleep(max(0, next_commit_at - time.monotonic()))
            with self.database:
                self.database.execute("BEGIN IMMEDIATE")
                put(
                    self.database,
                    "agent",
                    "continuous-agent",
                    {"id": "continuous-agent", "kind": "agent", "tokensUsed": index + 1},
                )
            commit_times.append(time.monotonic())
        burst_ended_at = time.monotonic()
        await receiver

        print(
            "CONTINUOUS_COMMIT_PUBLICATIONS",
            {
                "commits": 40,
                "intervalMs": 50,
                "burstMs": round((burst_ended_at - started_at) * 1000),
                "commitCount": len(commit_times),
                "publications": len(events),
                "finalSeq": events[-1][1].resourceVersions[0].revision,
            },
        )
        self.assertGreaterEqual(len(events), 3)
        self.assertEqual(len(commit_times), 40)
        self.assertLess(burst_ended_at - started_at, 2.5)
        self.assertLess(events[0][0] - started_at, 0.8)
        self.assertTrue(any(at < burst_ended_at for at, _event in events))
        for published_at, event in events:
            sequences = event.resourceVersions[0].entitySequences or []
            self.assertTrue(sequences)
            self.assertLess(
                published_at - commit_times[min(sequences) - 1],
                0.8,
            )
        final_at, final_event = events[-1]
        self.assertLess(final_at - burst_ended_at, 0.35)
        self.assertEqual(final_event.resourceVersions[0].revision, 40)
        self.assertEqual(final_event.resourceVersions[0].entitySequences, [40])


if __name__ == "__main__":
    unittest.main()
