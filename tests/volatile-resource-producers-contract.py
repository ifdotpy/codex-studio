#!/usr/bin/env python3
"""Domain producer publications reach registered resource subscribers."""
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

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

from codex_costs import CostReader, _publish_costs
from codex_session_costs import SessionCostReader
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


if __name__ == "__main__":
    unittest.main()
