#!/usr/bin/env python3
"""Domain producer publications reach registered resource subscribers."""
import asyncio
from contextlib import closing, contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
sys.path.insert(0, str(ROOT / "scripts"))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from codex_costs import CostReader, _publish_costs
from codex_session_costs import SessionCostReader
import codex_session_costs
from codex_token_rate import TokenRates
from codex_voice import VoiceStore
from codex_worktree_disk import WorktreeDiskScanner, _publish_worktree_disk
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    CostsResource,
    PanelResource,
    ResourceRef,
    SessionCostResource,
    VoiceResource,
    WorktreeDiskResource,
)


class VoiceRuntime:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.lock = threading.RLock()

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.root / "voice.sqlite3")
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    @staticmethod
    def agent(agent_id: str) -> dict[str, object]:
        return {"id": agent_id, "isLead": True, "epoch": 1, "autoWake": True}


def publish_voice(root: Path) -> None:
    voice = VoiceStore(VoiceRuntime(root))
    voice.record("agent-a", "", "event-a", "event", "updated")


def refresh_costs(root: Path) -> None:
    report = [{
        "provider": "codex",
        "source": "local",
        "currencyCode": "USD",
        "sessionCostUSD": 12.5,
        "last30DaysCostUSD": 100.25,
        "sessionTokens": 1000,
        "last30DaysTokens": 4000,
        "historyCoverageIsEstablished": True,
        "coverage": {"priced": 2, "unpriced": 0},
        "daily": [{
            "date": "2026-10-04",
            "modelsUsed": ["known"],
            "modelBreakdowns": [{"modelName": "known", "cost": 12.5, "totalTokens": 1000}],
        }],
    }]
    output = json.dumps(report)
    command = [sys.executable, "-c", f"import sys; sys.stdout.write({output!r})"]
    reader = CostReader(root, command=lambda: command,
                        on_change=lambda: _publish_costs(root))
    try:
        reader._refresh()
        if reader.snapshot()["data"] is None:
            raise AssertionError("cost refresh did not populate its committed cache")
    finally:
        reader.close()


def refresh_session_cost(root: Path) -> None:
    reader = SessionCostReader(root / "canvas.sqlite3", pricing=None, state_root=root)

    def complete(_agent_id, team_root, *, refresh):
        if not refresh:
            raise AssertionError("integration test must exercise the async refresh path")
        reader.cache[team_root] = (100.0, {"rootId": team_root, "totalUSD": 1.0}, {})

    with patch.object(reader, "_compute_shared", side_effect=complete):
        reader._background_refresh("agent-a", "agent-a")


def publish_worktree_scan(root: Path) -> None:
    worktree = root / "repo" / ".worktrees" / "codex-agents" / "agent-a"
    worktree.mkdir(parents=True)
    (worktree / "tracked.txt").write_text("measured")
    with closing(sqlite3.connect(root / "canvas.sqlite3")) as db, db:
        db.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        db.execute("INSERT INTO runtime_agents VALUES (?, ?)", (
            "agent-a",
            json.dumps({"id": "agent-a", "isLead": False, "worktree": True,
                        "cwd": str(worktree)}),
        ))
    WorktreeDiskScanner(root, pause=lambda _seconds: None).scan_once()


def resource_refs() -> list[ResourceRef]:
    return [
        ResourceRef(CostsResource(kind="costs")),
        ResourceRef(SessionCostResource(kind="session-cost", agentId="agent-a")),
        ResourceRef(WorktreeDiskResource(kind="worktree-disk", agentId="agent-a")),
        ResourceRef(VoiceResource(kind="voice", agentId="agent-a")),
        ResourceRef(PanelResource(kind="panel", agentId="agent-a")),
    ]


class VolatileResourceProducerContracts(unittest.IsolatedAsyncioTestCase):
    async def test_producers_publish_refs_to_subscribed_hub(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            loop = asyncio.get_running_loop()
            hub = ResourceHub("workspace-a")
            register_resource_hub(root, hub)
            subscription = hub.subscribe(resource_refs(), loop=loop)
            try:
                refresh_costs(root)
                refresh_session_cost(root)
                publish_voice(root)
                publish_worktree_scan(root)

                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(
                    {resource.root.kind for resource in event.resources},
                    {"costs", "session-cost", "worktree-disk", "voice"},
                )
            finally:
                subscription.close()
                unregister_resource_hub(root, hub)

    async def test_producers_are_safe_before_hub_registration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _publish_costs(root)
            refresh_session_cost(root)
            _publish_worktree_disk(root, ["agent-a"])
            publish_voice(root)
            publish_worktree_scan(root)
            rates = TokenRates(clock=lambda: 100.0, state_dir=root)
            rates.observe(
                {"id": "agent-a", "rootId": "agent-a", "threadId": "thread-a",
                 "inFlight": True, "turnId": "turn-a"},
                "turn/started", {"turnId": "turn-a"}, "account-a", "connection-a", 100.0,
            )

    async def test_shared_session_refresh_notifies_each_waiting_agent(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            loop = asyncio.get_running_loop()
            hub = ResourceHub("workspace-a")
            register_resource_hub(root, hub)
            subscription = hub.subscribe(
                [
                    ResourceRef(SessionCostResource(kind="session-cost", agentId="lead")),
                    ResourceRef(SessionCostResource(kind="session-cost", agentId="worker-a")),
                ],
                loop=loop,
            )
            reader = SessionCostReader(root / "canvas.sqlite3", pricing=None, state_root=root)
            compute_started = threading.Event()
            release_compute = threading.Event()
            both_published = threading.Event()
            published: list[str] = []
            published_lock = threading.Lock()
            original_publish = codex_session_costs._publish_session_cost
            previous_cache = (
                reader.clock(),
                {"rootId": "lead", "totalUSD": 2.0},
                {},
            )
            reader.cache["lead"] = previous_cache

            def compute(_agent_id: str, team_root: str, *, refresh: bool) -> dict[str, object]:
                if not refresh:
                    raise AssertionError("expected a shared background refresh")
                compute_started.set()
                if not release_compute.wait(2):
                    raise TimeoutError("test did not release shared session-cost refresh")
                self.assertEqual(team_root, "lead")
                return previous_cache[1]

            def publish(state_dir: str | Path, agent_id: str) -> None:
                original_publish(state_dir, agent_id)
                with published_lock:
                    published.append(agent_id)
                    if len(published) == 2:
                        both_published.set()

            try:
                with patch.object(reader, "_compute_shared", side_effect=compute), patch.object(
                    codex_session_costs, "_publish_session_cost", side_effect=publish
                ):
                    self.assertTrue(reader._start_refresh("lead", "lead"))
                    self.assertTrue(await asyncio.to_thread(compute_started.wait, 1))
                    self.assertFalse(reader._start_refresh("worker-a", "lead"))
                    release_compute.set()
                    self.assertTrue(await asyncio.to_thread(both_published.wait, 1))

                received: set[str] = set()
                while received != {"lead", "worker-a"}:
                    event = await subscription.next_event(timeout=1)
                    assert event is not None
                    received.update(
                        resource.root.agentId
                        for resource in event.resources
                        if resource.root.kind == "session-cost"
                    )
                self.assertEqual(set(published), {"lead", "worker-a"})
                self.assertEqual(reader.refreshing, set())
                self.assertEqual(reader.refresh_waiters, {})
            finally:
                release_compute.set()
                subscription.close()
                unregister_resource_hub(root, hub)

    async def test_distinct_session_roots_queue_and_complete_without_another_request(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            loop = asyncio.get_running_loop()
            hub = ResourceHub("workspace-a")
            register_resource_hub(root, hub)
            subscription = hub.subscribe(
                [
                    ResourceRef(SessionCostResource(kind="session-cost", agentId="agent-a")),
                    ResourceRef(SessionCostResource(kind="session-cost", agentId="agent-b")),
                ],
                loop=loop,
            )
            reader = SessionCostReader(root / "canvas.sqlite3", pricing=None, state_root=root)
            first_started = threading.Event()
            release_first = threading.Event()
            second_started = threading.Event()
            release_second = threading.Event()
            both_published = threading.Event()
            published: list[str] = []
            published_lock = threading.Lock()
            original_publish = codex_session_costs._publish_session_cost

            def compute(agent_id: str, team_root: str, *, refresh: bool) -> dict[str, object]:
                self.assertTrue(refresh)
                if team_root == "root-a":
                    first_started.set()
                    if not release_first.wait(2):
                        raise TimeoutError("test did not release first root refresh")
                elif team_root == "root-b":
                    second_started.set()
                    if not release_second.wait(2):
                        raise TimeoutError("test did not release queued root refresh")
                else:
                    raise AssertionError(f"unexpected root {team_root}")
                result = {"rootId": team_root, "totalUSD": 1.0}
                with reader.lock:
                    reader.cache[team_root] = (reader.clock(), result, {})
                return result

            def publish(state_dir: str | Path, agent_id: str) -> None:
                original_publish(state_dir, agent_id)
                with published_lock:
                    published.append(agent_id)
                    if len(published) == 2:
                        both_published.set()

            try:
                with patch.object(reader, "_compute_shared", side_effect=compute), patch.object(
                    codex_session_costs, "_publish_session_cost", side_effect=publish
                ):
                    self.assertTrue(reader._start_refresh("agent-a", "root-a"))
                    self.assertTrue(await asyncio.to_thread(first_started.wait, 1))
                    self.assertTrue(reader._start_refresh("agent-b", "root-b"))
                    self.assertEqual(reader.active_refresh_root, "root-a")
                    self.assertEqual(list(reader.refresh_queue), ["root-b"])
                    self.assertEqual(reader.refreshing, {"root-a", "root-b"})

                    release_first.set()
                    self.assertTrue(await asyncio.to_thread(second_started.wait, 1))
                    self.assertEqual(reader.active_refresh_root, "root-b")
                    self.assertEqual(list(reader.refresh_queue), [])
                    release_second.set()
                    self.assertTrue(await asyncio.to_thread(both_published.wait, 1))

                received: set[str] = set()
                while received != {"agent-a", "agent-b"}:
                    event = await subscription.next_event(timeout=1)
                    assert event is not None
                    received.update(
                        resource.root.agentId
                        for resource in event.resources
                        if resource.root.kind == "session-cost"
                    )
                self.assertEqual(set(published), {"agent-a", "agent-b"})
                self.assertEqual(reader.refreshing, set())
                self.assertEqual(reader.refresh_waiters, {})
                self.assertEqual(reader.refresh_queue, {})
            finally:
                release_first.set()
                release_second.set()
                subscription.close()
                unregister_resource_hub(root, hub)

if __name__ == "__main__":
    unittest.main()
